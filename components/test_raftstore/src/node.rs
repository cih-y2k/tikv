// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use tempdir::TempDir;

use kvproto::metapb;
use kvproto::raft_cmdpb::*;
use kvproto::raft_serverpb::{self, RaftMessage};
use raft::eraftpb::MessageType;
use raft::SnapshotStatus;

use tikv::config::TiKvConfig;
use tikv::import::SSTImporter;
use tikv::raftstore::coprocessor::CoprocessorHost;
use tikv::raftstore::store::fsm::{create_raft_batch_system, SendCh};
use tikv::raftstore::store::*;
use tikv::raftstore::Result;
use tikv::server::transport::{RaftStoreRouter, ServerRaftStoreRouter};
use tikv::server::Node;
use tikv::util::collections::{HashMap, HashSet};
use tikv::util::worker::{FutureWorker, Worker};

use super::*;

pub struct ChannelTransportCore {
    snap_paths: HashMap<u64, (SnapManager, TempDir)>,
    routers: HashMap<u64, SimulateTransport<Msg, ServerRaftStoreRouter>>,
}

#[derive(Clone)]
pub struct ChannelTransport {
    core: Arc<Mutex<ChannelTransportCore>>,
}

impl ChannelTransport {
    pub fn new() -> ChannelTransport {
        ChannelTransport {
            core: Arc::new(Mutex::new(ChannelTransportCore {
                snap_paths: HashMap::default(),
                routers: HashMap::default(),
            })),
        }
    }
}

impl Channel<RaftMessage> for ChannelTransport {
    fn send(&self, msg: RaftMessage) -> Result<()> {
        let from_store = msg.get_from_peer().get_store_id();
        let to_store = msg.get_to_peer().get_store_id();
        let to_peer_id = msg.get_to_peer().get_id();
        let region_id = msg.get_region_id();
        let is_snapshot = msg.get_message().get_msg_type() == MessageType::MsgSnapshot;

        if msg.get_message().get_msg_type() == MessageType::MsgSnapshot {
            let snap = msg.get_message().get_snapshot();
            let key = SnapKey::from_snap(snap).unwrap();
            let from = match self.core.lock().unwrap().snap_paths.get(&from_store) {
                Some(p) => {
                    p.0.register(key.clone(), SnapEntry::Sending);
                    p.0.get_snapshot_for_sending(&key).unwrap()
                }
                None => return Err(box_err!("missing temp dir for store {}", from_store)),
            };
            let to = match self.core.lock().unwrap().snap_paths.get(&to_store) {
                Some(p) => {
                    p.0.register(key.clone(), SnapEntry::Receiving);
                    let data = msg.get_message().get_snapshot().get_data();
                    p.0.get_snapshot_for_receiving(&key, data).unwrap()
                }
                None => return Err(box_err!("missing temp dir for store {}", to_store)),
            };

            defer!({
                let core = self.core.lock().unwrap();
                core.snap_paths[&from_store]
                    .0
                    .deregister(&key, &SnapEntry::Sending);
                core.snap_paths[&to_store]
                    .0
                    .deregister(&key, &SnapEntry::Receiving);
            });

            copy_snapshot(from, to)?;
        }

        let core = self.core.lock().unwrap();

        match core.routers.get(&to_store) {
            Some(h) => {
                h.send_raft_msg(msg)?;
                if is_snapshot {
                    // should report snapshot finish.
                    core.routers[&from_store]
                        .report_snapshot_status(region_id, to_peer_id, SnapshotStatus::Finish)
                        .unwrap();
                }
                Ok(())
            }
            _ => Err(box_err!("missing sender for store {}", to_store)),
        }
    }
}

type SimulateChannelTransport = SimulateTransport<RaftMessage, ChannelTransport>;

pub struct NodeCluster {
    trans: ChannelTransport,
    pd_client: Arc<TestPdClient>,
    nodes: HashMap<u64, Node<TestPdClient>>,
    simulate_trans: HashMap<u64, SimulateChannelTransport>,
    post_create_coprocessor_host: Option<Box<Fn(u64, &mut CoprocessorHost)>>,
}

impl NodeCluster {
    pub fn new(pd_client: Arc<TestPdClient>) -> NodeCluster {
        NodeCluster {
            trans: ChannelTransport::new(),
            pd_client,
            nodes: HashMap::default(),
            simulate_trans: HashMap::default(),
            post_create_coprocessor_host: None,
        }
    }
}

impl NodeCluster {
    #[allow(dead_code)]
    pub fn get_node_router(&self, node_id: u64) -> SimulateTransport<Msg, ServerRaftStoreRouter> {
        self.trans
            .core
            .lock()
            .unwrap()
            .routers
            .get(&node_id)
            .cloned()
            .unwrap()
    }

    // Set a function that will be invoked after creating each CoprocessorHost. The first argument
    // of `op` is the node_id.
    // Set this before invoking `run_node`.
    pub fn post_create_coprocessor_host(&mut self, op: Box<Fn(u64, &mut CoprocessorHost)>) {
        self.post_create_coprocessor_host = Some(op)
    }
}

impl Simulator for NodeCluster {
    fn run_node(
        &mut self,
        node_id: u64,
        cfg: TiKvConfig,
        engines: Option<Engines>,
    ) -> (u64, Engines, Option<TempDir>) {
        assert!(node_id == 0 || !self.nodes.contains_key(&node_id));
        let (router, system) = create_raft_batch_system(&cfg.raft_store);
        let pd_worker = FutureWorker::new("test-pd-worker");

        // Create localreader.
        let local_reader = Worker::new("test-local-reader");
        let local_ch = local_reader.scheduler();

        let simulate_trans = SimulateTransport::new(self.trans.clone());
        let mut node = Node::new(
            system,
            &cfg.server,
            &cfg.raft_store,
            Arc::clone(&self.pd_client),
        );

        // Create engine
        let (engines, path) = create_test_engine(engines, node.get_sendch(), &cfg);

        let (snap_mgr, tmp) = if node_id == 0
            || !self
                .trans
                .core
                .lock()
                .unwrap()
                .snap_paths
                .contains_key(&node_id)
        {
            let tmp = TempDir::new("test_cluster").unwrap();
            let snap_mgr = SnapManager::new(tmp.path().to_str().unwrap(), Some(node.get_sendch()));
            (snap_mgr, Some(tmp))
        } else {
            let trans = self.trans.core.lock().unwrap();
            let &(ref snap_mgr, _) = &trans.snap_paths[&node_id];
            (snap_mgr.clone(), None)
        };

        // Create coprocessor.
        let mut coprocessor_host = CoprocessorHost::new(cfg.coprocessor, node.get_sendch());

        if let Some(f) = self.post_create_coprocessor_host.as_ref() {
            f(node_id, &mut coprocessor_host);
        }

        let importer = {
            let dir = Path::new(engines.kv.path()).join("import-sst");
            Arc::new(SSTImporter::new(dir).unwrap())
        };

        node.start(
            engines.clone(),
            simulate_trans.clone(),
            snap_mgr.clone(),
            pd_worker,
            local_reader,
            coprocessor_host,
            importer,
        ).unwrap();
        assert!(
            engines
                .kv
                .get_msg::<metapb::Region>(keys::PREPARE_BOOTSTRAP_KEY)
                .unwrap()
                .is_none()
        );
        assert!(node_id == 0 || node_id == node.id());
        debug!(
            "node_id: {} tmp: {:?}",
            node_id,
            tmp.as_ref().map(|p| p.path().to_str().unwrap().to_owned())
        );
        if let Some(tmp) = tmp {
            self.trans
                .core
                .lock()
                .unwrap()
                .snap_paths
                .insert(node.id(), (snap_mgr, tmp));
        }

        let node_id = node.id();
        let router = ServerRaftStoreRouter::new(node.get_sendch(), router.clone(), local_ch);
        self.trans
            .core
            .lock()
            .unwrap()
            .routers
            .insert(node_id, SimulateTransport::new(router));
        self.nodes.insert(node_id, node);
        self.simulate_trans.insert(node_id, simulate_trans);

        (node_id, engines, path)
    }

    fn get_snap_dir(&self, node_id: u64) -> String {
        self.trans.core.lock().unwrap().snap_paths[&node_id]
            .1
            .path()
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn stop_node(&mut self, node_id: u64) {
        if let Some(mut node) = self.nodes.remove(&node_id) {
            node.stop().unwrap();
        }
        self.trans
            .core
            .lock()
            .unwrap()
            .routers
            .remove(&node_id)
            .unwrap();
    }

    fn get_node_ids(&self) -> HashSet<u64> {
        self.nodes.keys().cloned().collect()
    }

    fn async_command_on_node(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        cb: Callback,
    ) -> Result<()> {
        if !self
            .trans
            .core
            .lock()
            .unwrap()
            .routers
            .contains_key(&node_id)
        {
            return Err(box_err!("missing sender for store {}", node_id));
        }

        let router = self
            .trans
            .core
            .lock()
            .unwrap()
            .routers
            .get(&node_id)
            .cloned()
            .unwrap();
        router.send_command(request, cb)
    }

    fn send_raft_msg(&mut self, msg: raft_serverpb::RaftMessage) -> Result<()> {
        self.trans.send(msg)
    }

    fn add_send_filter(&mut self, node_id: u64, filter: SendFilter) {
        self.simulate_trans
            .get_mut(&node_id)
            .unwrap()
            .add_filter(filter);
    }

    fn clear_send_filters(&mut self, node_id: u64) {
        self.simulate_trans
            .get_mut(&node_id)
            .unwrap()
            .clear_filters();
    }

    fn add_recv_filter(&mut self, node_id: u64, filter: RecvFilter) {
        let mut trans = self.trans.core.lock().unwrap();
        trans.routers.get_mut(&node_id).unwrap().add_filter(filter);
    }

    fn clear_recv_filters(&mut self, node_id: u64) {
        let mut trans = self.trans.core.lock().unwrap();
        trans.routers.get_mut(&node_id).unwrap().clear_filters();
    }

    fn get_store_sendch(&self, node_id: u64) -> Option<SendCh> {
        self.nodes.get(&node_id).map(|node| node.get_sendch())
    }
}

pub fn new_node_cluster(id: u64, count: usize) -> Cluster<NodeCluster> {
    let pd_client = Arc::new(TestPdClient::new(id, false));
    let sim = Arc::new(RwLock::new(NodeCluster::new(Arc::clone(&pd_client))));
    Cluster::new(id, count, sim, pd_client)
}

pub fn new_incompatible_node_cluster(id: u64, count: usize) -> Cluster<NodeCluster> {
    let pd_client = Arc::new(TestPdClient::new(id, true));
    let sim = Arc::new(RwLock::new(NodeCluster::new(Arc::clone(&pd_client))));
    Cluster::new(id, count, sim, pd_client)
}
