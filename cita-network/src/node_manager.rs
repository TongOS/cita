// Copyrighttape Technologies LLC.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::cita_protocol::{
    pubsub_message_to_network_message, NetMessageUnit, CONSENSUS_STR, CONSENSUS_TTL_NUM,
};
use crate::config::NetConfig;
use crate::mq_agent::{MqAgentClient, PubMessage};
use crate::p2p_protocol::transfer::TRANSFER_PROTOCOL_ID;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use cita_types::{clean_0x, Address};
use fnv::FnvHashMap as HashMap;
use libproto::request::Request as ProtoRequest;
use libproto::router::{MsgType, RoutingKey, SubModules};
use libproto::Call;
use libproto::{routing_key, Message};
use libproto::{Message as ProtoMessage, TryInto};
use notify::DebouncedEvent;
use openssl::nid::Nid;
use openssl::stack::Stack;
use openssl::x509::{store::X509StoreBuilder, X509StoreContext, X509};
use pubsub::channel::{select, tick, unbounded, Receiver, Sender};
use rand::{thread_rng, Rng};
use std::fs::File;
use std::io::Read;
use std::str;
use std::str::FromStr;
use std::sync::mpsc::Receiver as StdReceiver;
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Into,
    io::Cursor,
    net::{SocketAddr, ToSocketAddrs},
    time::{Duration, Instant},
};
use tentacle::{
    service::{DialProtocol, ServiceControl, SessionType, TargetSession},
    utils::socketaddr_to_multiaddr,
    SessionId,
};
use util::sha3;
use uuid::Uuid;

pub const DEFAULT_MAX_CONNECTS: usize = 666;
pub const DEFAULT_MAX_KNOWN_ADDRS: usize = 1000;
pub const DEFAULT_PORT: usize = 4000;
pub const CHECK_CONNECTED_NODES: Duration = Duration::from_secs(3);
// Check the certificate time validity in each 12 hour.
pub const CHECK_CERT_PERIOD: Duration = Duration::from_secs(12 * 3600);
// Update Certificate Revoke List for each 100 block period.
pub const UPDATE_CRL_PERIOD: u64 = 100;

// Score uses to manage known_nodes list. If a node has too low score, do not dial it again.
// Maybe some complex algorithm can be designed later. But for now, just keeps as simple as below:
//  1. Deducts 10 score for each Dial;
//  2. Deducts 25 score for each Dial Error;
//  3. Deducts 20 score for each Disconnected by server;
//  4. Add 5 score for every dialing round if the node keep on line; so If a node keep on line,
//     it will get FULL_SCORE very fast.
//  5. Gives a Time sugar score (2 : nodes was configured in config file, and 1 : nodes was
//     discovered by P2P framework ) when a node's score less than MIN_DIALING_SCORE;

// A new node come into known_nodes list has a FULL_SCORE.
pub const FULL_SCORE: i32 = 100;
// Score lower than MIN_DIALING_SCORE, stop dialing.
pub const MIN_DIALING_SCORE: i32 = 60;
// A node needs DIALING_SCORE for every dial.
pub const DIALING_SCORE: i32 = 10;
// A node connected successfully, can get SUCCESS_DIALING_SCORE.
pub const SUCCESS_DIALING_SCORE: i32 = 10;
// A node is refused by server, should need REFUSED_SCORE each time.
pub const REFUSED_SCORE: i32 = 20;
// A node is dialed error by client, should need DIALED_ERROR_SCORE each time.
pub const DIALED_ERROR_SCORE: i32 = 25;
// A node is dialed error by client, should need DIALED_ERROR_SCORE each time.
pub const KEEP_ON_LINE_SCORE: i32 = 5;

fn encode_to_vec(name: &[u8]) -> Vec<u8> {
    sha3::keccak256(name)[0..4].to_vec()
}

fn create_request() -> ProtoRequest {
    let request_id = Uuid::new_v4().as_bytes().to_vec();
    let mut request = ProtoRequest::new();

    request.set_request_id(request_id);
    request
}

#[derive(Debug, PartialEq)]
pub enum NodeSource {
    FromConfig,
    FromDiscovery,
}

#[derive(Debug)]
pub struct NodeStatus {
    // score: Score for a node, it will affect whether the node will be chosen to dail again,
    // or be deleted from the known_addresses list. But for now, it useless.
    pub score: i32,

    // session_id: Indicates that this node has been connected to a session. 'None' for has not
    // connected yet.
    pub session_id: Option<SessionId>,
    pub node_src: NodeSource,
}

impl NodeStatus {
    pub fn new(score: i32, session_id: Option<SessionId>, node_src: NodeSource) -> Self {
        NodeStatus {
            score,
            session_id,
            node_src,
        }
    }
}

#[derive(Debug)]
pub struct SessionInfo {
    pub ty: SessionType,
    pub addr: SocketAddr,
}

impl SessionInfo {
    pub fn new(ty: SessionType, addr: SocketAddr) -> Self {
        SessionInfo { ty, addr }
    }
}

pub struct ConnectedInfo {
    // Real linked addr
    pub conn_addr: SocketAddr,
    // Outbound addr transformed from Inbound addr
    pub trans_addr: Option<SocketAddr>,
    pub node_crt: Option<X509>,
}

impl ConnectedInfo {
    pub fn new(
        conn_addr: SocketAddr,
        trans_addr: Option<SocketAddr>,
        node_crt: Option<X509>,
    ) -> Self {
        ConnectedInfo {
            conn_addr,
            trans_addr,
            node_crt,
        }
    }
}

#[derive(Default, Debug)]
pub struct ConsensusNodeTopology {
    pub linked_nodes: BTreeSet<Address>,
    pub validator_nodes: BTreeSet<Address>,
    //pub consensus_threshold_linked : bool,
    pub consensus_all_linked: bool,
    pub height: u64,
}

impl ConsensusNodeTopology {
    pub fn new(self_address: Address) -> ConsensusNodeTopology {
        let mut top = ConsensusNodeTopology::default();
        top.linked_nodes.insert(self_address);
        top
    }

    fn validator_subset_linked(&self) -> bool {
        self.validator_nodes.is_subset(&self.linked_nodes)
    }

    pub fn update_validators(&mut self, height: u64, validators: BTreeSet<Address>) {
        if height < self.height || validators == self.validator_nodes {
            debug!("No need update validator height {} self height {} validator {:?} self validator {:?}",
            height,self.height,validators,self.validator_nodes);

            if height > self.height {
                self.height = height;
            }
            return;
        }
        self.validator_nodes = validators;
        self.consensus_all_linked = self.validator_subset_linked();
    }

    pub fn add_linked_nodes(&mut self, linked_node: Address) {
        if self.linked_nodes.insert(linked_node) {
            self.consensus_all_linked = self.validator_subset_linked();
        }
    }

    pub fn del_linked_nodes(&mut self, linked_node: &Address) {
        if self.linked_nodes.remove(linked_node) {
            self.consensus_all_linked = self.validator_subset_linked();
        }
    }

    pub fn consensus_all_linked(&self) -> bool {
        self.consensus_all_linked
    }
    //pub fn consensus_threshold_linked(&self) -> bool {self.consensus_threshold_linked}
}

pub struct NodesManager {
    mq_client: MqAgentClient,
    known_addrs: HashMap<SocketAddr, NodeStatus>,
    config_addrs: BTreeMap<String, Option<SocketAddr>>,

    connected_addrs: BTreeMap<SessionId, ConnectedInfo>,
    pending_connected_addrs: BTreeMap<SessionId, SessionInfo>,

    connected_peer_keys: BTreeMap<Address, SessionId>,

    check_connected_nodes: Receiver<Instant>,
    check_cert: Receiver<Instant>,
    max_connects: usize,
    nodes_manager_client: NodesManagerClient,
    nodes_manager_service_receiver: Receiver<NodesManagerMessage>,
    service_ctrl: Option<ServiceControl>,
    peer_key: Address,

    gossip_key_version: HashMap<Address, u64>,
    consensus_topology: ConsensusNodeTopology,

    self_version: u64,

    dialing_node: Option<SocketAddr>,
    self_addr: Option<SocketAddr>,

    root_crt: Option<X509>,
    node_crt: Option<Vec<u8>>,
    enable_ca: bool,
    get_crl_point: u64,
    crl: Vec<Address>,
}

impl NodesManager {
    fn new(peer_key: Address, mq_client: MqAgentClient) -> NodesManager {
        let (tx, rx) = unbounded();
        let ticker = tick(CHECK_CONNECTED_NODES);
        let check_cert_ticker = tick(CHECK_CERT_PERIOD);
        let client = NodesManagerClient { sender: tx };

        NodesManager {
            mq_client,
            check_connected_nodes: ticker,
            check_cert: check_cert_ticker,
            known_addrs: HashMap::default(),
            config_addrs: BTreeMap::default(),
            connected_addrs: BTreeMap::default(),
            connected_peer_keys: BTreeMap::default(),
            pending_connected_addrs: BTreeMap::default(),
            max_connects: DEFAULT_MAX_CONNECTS,
            nodes_manager_client: client,
            nodes_manager_service_receiver: rx,
            service_ctrl: None,
            peer_key,
            dialing_node: None,
            self_addr: None,
            gossip_key_version: HashMap::default(),
            self_version: 0,
            consensus_topology: ConsensusNodeTopology::new(peer_key),
            root_crt: None,
            node_crt: None,
            enable_ca: false,
            get_crl_point: 0,
            crl: Vec::default(),
        }
    }

    pub fn from_config(cfg: NetConfig, key: Address, mq_client: MqAgentClient) -> Self {
        let mut node_mgr = NodesManager::new(key, mq_client);
        let max_connects = cfg.max_connects.unwrap_or(DEFAULT_MAX_CONNECTS);
        node_mgr.max_connects = max_connects;
        node_mgr.peer_key = key;
        node_mgr.enable_ca = cfg.enable_ca.unwrap_or(false);

        // If enable certificate authority, try to read root_ca and node_ca
        if node_mgr.enable_ca {
            let mut file = File::open("root.crt".to_owned())
                .expect("Needs a root certificate file in CA mode!");
            let mut cert = vec![];
            file.read_to_end(&mut cert)
                .expect("Root certificate file reads error!");
            let root_cert = X509::from_pem(cert.as_ref())
                .expect("Root certificate to X509 struct error! Changes the file to X509 format?");
            node_mgr.root_crt = Some(root_cert);

            file = File::open("node.crt".to_owned())
                .expect("Needs a node certificate file in CA mode!");
            cert = vec![];
            file.read_to_end(&mut cert)
                .expect("Node certificate file reads error!");
            node_mgr.node_crt = Some(cert);
        }

        if let Some(cfg_addrs) = cfg.peers {
            for addr in cfg_addrs {
                if let (Some(ip), Some(port)) = (addr.ip, addr.port) {
                    let addr_str = format!("{}:{}", ip, port);
                    node_mgr.config_addrs.insert(addr_str, None);
                } else {
                    warn!("[NodeManager] ip(host) & port 'MUST' be set in peers.");
                }
            }
        } else {
            warn!("[NodeManager] Does not set any peers in config file!");
        }
        node_mgr
    }

    pub fn notify_config_change(
        rx: StdReceiver<DebouncedEvent>,
        node_client: NodesManagerClient,
        fname: String,
    ) {
        loop {
            match rx.recv() {
                Ok(event) => match event {
                    DebouncedEvent::Create(path_buf) | DebouncedEvent::Write(path_buf) => {
                        if path_buf.is_file() {
                            let file_name = path_buf.file_name().unwrap().to_str().unwrap();
                            if file_name == fname {
                                info!("file {} changed, will auto reload!", file_name);

                                let config = NetConfig::new(file_name);
                                if let Some(peers) = config.peers {
                                    let mut addr_strs = Vec::new();
                                    for addr in peers {
                                        if let (Some(ip), Some(port)) = (addr.ip, addr.port) {
                                            addr_strs.push(format!("{}:{}", ip, port));
                                        }
                                    }
                                    node_client.fix_modified_config(ModifiedConfigPeersReq::new(
                                        addr_strs,
                                    ));
                                }
                            }
                        }
                    }
                    _ => trace!("file notify event: {:?}", event),
                },
                Err(e) => warn!("watch error: {:?}", e),
            }
        }
    }

    // clippy
    #[allow(clippy::drop_copy, clippy::zero_ptr)]
    pub fn run(&mut self) {
        loop {
            select! {
                recv(self.nodes_manager_service_receiver) -> msg => {
                    match msg {
                        Ok(data) => {
                            data.handle(self);
                        },
                        Err(err) => error!("[NodeManager] Receive data error {:?}", err),
                    }
                }
                recv(self.check_connected_nodes) -> _ => {
                    self.dial_nodes();
                }
                recv(self.check_cert) -> _ => {
                    self.check_cert();
                }
            }
        }
    }

    pub fn client(&self) -> NodesManagerClient {
        self.nodes_manager_client.clone()
    }

    pub fn check_cert(&mut self) {
        if self.enable_ca {
            info!("Check each certifcate in connecting nodes for its time validity.");

            for (key, value) in self.connected_addrs.iter() {
                if let Some(ref root_crt) = self.root_crt {
                    // make sure the certificate has been writen
                    if let Some(ref cert) = value.node_crt {
                        if let Err(e) = verify_crt(root_crt, cert) {
                            error!(
                                "Cerificate verified error: {:?}, and disconnect the session: {:?}",
                                e, key
                            );
                            error!("check if the certificate has expired!");

                            if let Some(ref mut ctrl) = self.service_ctrl {
                                let _ = ctrl.disconnect(*key);
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn dial_nodes(&mut self) {
        if let Some(dialing_node) = self.dialing_node {
            info!(
                "[NodeManager] Dialing node: {:?}, waiting for next round.",
                dialing_node
            );
            return;
        }
        self.translate_address();

        // If connected node has not reach MAX, select a node from known_addrs to dial.
        if self.connected_addrs.len() < self.max_connects {
            let mut socks: Vec<_> = self.known_addrs.keys().cloned().collect();
            thread_rng().shuffle(&mut socks);

            for key in socks {
                let value = self.known_addrs.get_mut(&key).unwrap();
                // Node has been connected
                if let Some(session_id) = value.session_id {
                    debug!(
                        "[NodeManager] Address {:?} has been connected on : {:?}.",
                        key, session_id
                    );

                    // Node keep on line, reward KEEP_ON_LINE_SCORE.
                    value.score = if (value.score + KEEP_ON_LINE_SCORE) > FULL_SCORE {
                        FULL_SCORE as i32
                    } else {
                        value.score + KEEP_ON_LINE_SCORE
                    };
                    continue;
                }

                if let Some(self_addr) = self.self_addr {
                    if key == self_addr {
                        debug!(
                            "[NodeManager] Trying to connected self: {:?}, skip it",
                            self_addr
                        );
                        continue;
                    }
                }

                // Score design prevents the client from dialing to a node all the time.
                if value.score < MIN_DIALING_SCORE {
                    debug!(
                        "[NodeManager] Address {:?} has to low score ({:?}) to dial.",
                        key, value.score
                    );

                    // The node will get time sugar, the nodes which in config file can get 2, and the
                    // other nodes which discovered by P2P can get 1.
                    value.score += if value.node_src == NodeSource::FromConfig {
                        2
                    } else {
                        1
                    };
                    continue;
                }

                // Dial this address
                if let Some(ref mut ctrl) = self.service_ctrl {
                    self.dialing_node = Some(key);
                    info!("Trying to dial: {:?}", self.dialing_node);
                    match ctrl.dial(socketaddr_to_multiaddr(key), DialProtocol::All) {
                        Ok(_) => {
                            // Need DIALING_SCORE for every dial.
                            value.score -= DIALING_SCORE;
                            debug!("[NodeManager] Dail success");
                        }
                        Err(err) => {
                            warn!("[NodeManager] Dail failed : {:?}", err);
                        }
                    }
                }
                break;
            }
        }

        debug!("[NodeManager] known_addrs info: {:?}", self.known_addrs);
        debug!(
            "[NodeManager] Address in connected : {:?}",
            self.connected_peer_keys
        );
    }

    pub fn set_service_task_sender(&mut self, ctrl: ServiceControl) {
        self.service_ctrl = Some(ctrl);
    }

    pub fn translate_address(&mut self) {
        for (key, value) in self.config_addrs.iter_mut() {
            // The address has translated.
            if value.is_some() {
                debug!("[NodeManager] The Address {:?} has been translated.", key);
                continue;
            }
            match key.to_socket_addrs() {
                Ok(mut result) => {
                    if let Some(socket_addr) = result.next() {
                        // An init node from config file, give it FULL_SCORE.
                        let node_status = NodeStatus::new(FULL_SCORE, None, NodeSource::FromConfig);
                        self.known_addrs.insert(socket_addr, node_status);
                        *value = Some(socket_addr);
                    } else {
                        error!("[NodeManager] Can not convert to socket address!");
                    }
                }
                Err(e) => {
                    error!(
                        "[NodeManager] Can not convert to socket address! error: {}",
                        e
                    );
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct NodesManagerClient {
    sender: Sender<NodesManagerMessage>,
}

impl NodesManagerClient {
    pub fn new(sender: Sender<NodesManagerMessage>) -> Self {
        NodesManagerClient { sender }
    }

    pub fn add_node(&self, req: AddNodeReq) {
        self.send_req(NodesManagerMessage::AddNodeReq(req));
    }

    pub fn dialed_error(&self, req: DialedErrorReq) {
        self.send_req(NodesManagerMessage::DialedErrorReq(req));
    }

    pub fn connected_self(&self, req: ConnectedSelfReq) {
        self.send_req(NodesManagerMessage::ConnectedSelf(req));
    }

    pub fn get_random_nodes(&self, req: GetRandomNodesReq) {
        self.send_req(NodesManagerMessage::GetRandomNodesReq(req));
    }

    pub fn pending_connected_node(&self, req: PendingConnectedNodeReq) {
        self.send_req(NodesManagerMessage::PendingConnectedNodeReq(req));
    }

    pub fn del_connected_node(&self, req: DelConnectedNodeReq) {
        self.send_req(NodesManagerMessage::DelConnectedNodeReq(req));
    }

    pub fn add_repeated_node(&self, req: AddRepeatedNodeReq) {
        self.send_req(NodesManagerMessage::AddRepeatedNode(req));
    }

    pub fn broadcast(&self, req: BroadcastReq) {
        self.send_req(NodesManagerMessage::Broadcast(req));
    }

    pub fn retrans_net_msg(&self, req: RetransNetMsgReq) {
        self.send_req(NodesManagerMessage::RetransNetMsg(req));
    }

    pub fn send_message(&self, req: SingleTxReq) {
        self.send_req(NodesManagerMessage::SingleTxReq(req));
    }

    pub fn get_peer_count(&self, req: GetPeerCountReq) {
        self.send_req(NodesManagerMessage::GetPeerCount(req));
    }

    pub fn get_peers_info(&self, req: GetPeersInfoReq) {
        self.send_req(NodesManagerMessage::GetPeersInfo(req));
    }

    pub fn network_init(&self, req: NetworkInitReq) {
        self.send_req(NodesManagerMessage::NetworkInit(req));
    }

    pub fn add_connected_node(&self, req: AddConnectedNodeReq) {
        self.send_req(NodesManagerMessage::AddConnectedNode(req));
    }

    pub fn fix_modified_config(&self, req: ModifiedConfigPeersReq) {
        self.send_req(NodesManagerMessage::ModifiedConfigPeers(req));
    }

    pub fn deal_rich_status(&self, req: DealRichStatusReq) {
        self.send_req(NodesManagerMessage::DealRichStatus(req));
    }

    pub fn update_crl(&self, req: UpdateCrlReq) {
        self.send_req(NodesManagerMessage::UpdateCrl(req));
    }

    fn send_req(&self, req: NodesManagerMessage) {
        if let Err(e) = self.sender.try_send(req) {
            warn!(
                "[NodesManager] Send message to node manager failed : {:?}",
                e
            );
        }
    }
}

// Define messages for NodesManager
pub enum NodesManagerMessage {
    AddNodeReq(AddNodeReq),
    DialedErrorReq(DialedErrorReq),
    GetRandomNodesReq(GetRandomNodesReq),
    PendingConnectedNodeReq(PendingConnectedNodeReq),
    DelConnectedNodeReq(DelConnectedNodeReq),
    Broadcast(BroadcastReq),
    RetransNetMsg(RetransNetMsgReq),
    SingleTxReq(SingleTxReq),
    GetPeerCount(GetPeerCountReq),
    NetworkInit(NetworkInitReq),
    AddConnectedNode(AddConnectedNodeReq),
    AddRepeatedNode(AddRepeatedNodeReq),
    ConnectedSelf(ConnectedSelfReq),
    GetPeersInfo(GetPeersInfoReq),
    ModifiedConfigPeers(ModifiedConfigPeersReq),
    DealRichStatus(DealRichStatusReq),
    UpdateCrl(UpdateCrlReq),
}

impl NodesManagerMessage {
    pub fn handle(self, service: &mut NodesManager) {
        match self {
            NodesManagerMessage::AddNodeReq(req) => req.handle(service),
            NodesManagerMessage::DialedErrorReq(req) => req.handle(service),
            NodesManagerMessage::GetRandomNodesReq(req) => req.handle(service),
            NodesManagerMessage::PendingConnectedNodeReq(req) => req.handle(service),
            NodesManagerMessage::DelConnectedNodeReq(req) => req.handle(service),
            NodesManagerMessage::Broadcast(req) => req.handle(service),
            NodesManagerMessage::SingleTxReq(req) => req.handle(service),
            NodesManagerMessage::GetPeerCount(req) => req.handle(service),
            NodesManagerMessage::NetworkInit(req) => req.handle(service),
            NodesManagerMessage::AddConnectedNode(req) => req.handle(service),
            NodesManagerMessage::AddRepeatedNode(req) => req.handle(service),
            NodesManagerMessage::ConnectedSelf(req) => req.handle(service),
            NodesManagerMessage::GetPeersInfo(req) => req.handle(service),
            NodesManagerMessage::ModifiedConfigPeers(req) => req.handle(service),
            NodesManagerMessage::RetransNetMsg(req) => req.handle(service),
            NodesManagerMessage::DealRichStatus(req) => req.handle(service),
            NodesManagerMessage::UpdateCrl(req) => req.handle(service),
        }
    }
}

#[derive(Default, Clone)]
pub struct InitMsg {
    pub chain_id: u64,
    pub peer_key: Address,
    pub node_crt: Option<Vec<u8>>,
}

impl Into<Vec<u8>> for InitMsg {
    fn into(self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut key_data: [u8; 20] = Default::default();
        let mut chain_id_data = vec![];
        chain_id_data.write_u64::<BigEndian>(self.chain_id).unwrap();
        self.peer_key.copy_to(&mut key_data[..]);

        out.extend_from_slice(&chain_id_data);
        out.extend_from_slice(&key_data);
        if let Some(cert) = self.node_crt {
            out.extend_from_slice(&cert);
        }
        out
    }
}

impl From<Vec<u8>> for InitMsg {
    fn from(data: Vec<u8>) -> InitMsg {
        let mut chain_id_data: [u8; 8] = Default::default();
        chain_id_data.copy_from_slice(&data[..8]);
        let mut chain_id_data = Cursor::new(chain_id_data);
        let chain_id = chain_id_data.read_u64::<BigEndian>().unwrap();
        let peer_key = Address::from_slice(&data[8..28]);

        info!("InitMsg data lenght : {}", data.len());
        if data.len() > 28 {
            InitMsg {
                chain_id,
                peer_key,
                node_crt: Some(data[28..].to_vec()),
            }
        } else {
            InitMsg {
                chain_id,
                peer_key,
                node_crt: None,
            }
        }
    }
}

fn verify_crt(root_crt: &X509, node_crt: &X509) -> Result<bool, String> {
    // Verify cerificate, see https://www.openssl.org/docs/man1.0.2/man1/verify.html
    let chain = Stack::new().unwrap();
    let mut store_bldr = X509StoreBuilder::new().unwrap();
    store_bldr.add_cert(root_crt.clone()).unwrap();
    let store = store_bldr.build();
    let mut context = X509StoreContext::new().unwrap();

    // verify_cert is an unsafe function, so allow the clippy error.
    #[allow(clippy::redundant_closure)]
    match context.init(&store, node_crt, &chain, |c| c.verify_cert()) {
        Ok(ret) => {
            if !ret {
                return Err("Verifty cerificate failed with unkown reason".to_owned());
            }
        }
        Err(e) => return Err(format!("Verifty cerificate failed with error : {:?}", e)),
    }
    Ok(true)
}

fn get_common_name(crt: &X509) -> Result<Address, String> {
    // Verify Common Name, should be equal to node address
    let subject = crt.subject_name();
    if subject.entries_by_nid(Nid::COMMONNAME).next().is_none() {
        return Err("Get common name error".to_owned());
    }

    let cn_str = str::from_utf8(
        subject
            .entries_by_nid(Nid::COMMONNAME)
            .next()
            .unwrap()
            .data()
            .as_slice(),
    )
    .map_err(|e| format!("Can not get Comman Name String: {:?}", e))?;
    Ok(Address::from_str(clean_0x(&cn_str))
        .map_err(|e| format!("Can not get node address from Comman Name String: {:?}", e))?)
}

// Also need to verify Common Name in CITA.
fn cita_verify_crt(
    root_crt: &X509,
    node_crt: &X509,
    claim_addr: &Address,
    crl: &[Address],
) -> Result<bool, String> {
    // Verify cerificate
    verify_crt(root_crt, node_crt)?;

    let addr = get_common_name(node_crt)?;
    if *claim_addr != addr {
        return Err(format!(
            "Address in common name({:?}) does not equal to self address({:?})",
            addr, *claim_addr
        ));
    }

    // Check for certificate revoke list
    for crl_addr in crl.iter() {
        if crl_addr.contains(&addr) {
            return Err("The certificate has been revoked!".to_owned());
        }
    }
    Ok(true)
}

fn handle_repeated_connect(
    session_id: SessionId,
    ty: SessionType,
    peer_key: &Address,
    service: &mut NodesManager,
) -> bool {
    if let Some(repeated_id) = service.connected_peer_keys.get(peer_key) {
        info!(
            "[NodeManager] New session [{:?}] repeated with [{:?}], disconnect this session.",
            session_id, *repeated_id
        );

        // It is a repeated_session, but not a repeated node.
        if let Some(dialing_addr) = service.dialing_node {
            if ty == SessionType::Outbound {
                if let Some(ref mut node_status) = service.known_addrs.get_mut(&dialing_addr) {
                    node_status.session_id = Some(*repeated_id);
                    node_status.score += SUCCESS_DIALING_SCORE;

                    let _ = service.connected_addrs.entry(*repeated_id).and_modify(|v| {
                        v.trans_addr = Some(dialing_addr);
                    });
                }
            }
        }

        if let Some(ref mut ctrl) = service.service_ctrl {
            let _ = ctrl.disconnect(session_id);
        }
        true
    } else {
        false
    }
}

fn handle_connect_self(
    session_id: SessionId,
    peer_key: &Address,
    service: &mut NodesManager,
) -> bool {
    if service.peer_key == *peer_key {
        if let Some(dialing_node) = service.dialing_node {
            debug!(
                "[NodeManager] Connected Self, Delete {:?} from know_addrs",
                dialing_node
            );
            service.self_addr = Some(dialing_node);
            if let Some(ref mut ctrl) = service.service_ctrl {
                let _ = ctrl.disconnect(session_id);
            }
        }
        true
    } else {
        false
    }
}

pub struct AddConnectedNodeReq {
    session_id: SessionId,
    ty: SessionType,
    init_msg: InitMsg,
}

impl AddConnectedNodeReq {
    pub fn new(session_id: SessionId, ty: SessionType, init_msg: InitMsg) -> Self {
        AddConnectedNodeReq {
            session_id,
            ty,
            init_msg,
        }
    }

    pub fn handle(self, service: &mut NodesManager) {
        // Repeated connected, it can a duplicated connected to the same node, or a duplicated
        // node connected to this server. But in either case, disconnect this session.
        // In P2P encrypted communication mode, the repeated connection will be detected by
        // P2P framework, handling this situation by sending a `AddRepeatedNodeReq` message to
        // NodesManager. See the `handle` in `AddRepeatedNodeReq` for more detail.
        let repeated_connect =
            handle_repeated_connect(self.session_id, self.ty, &self.init_msg.peer_key, service);

        // Connected self, disconnected the session.
        // In P2P encrypted communication mode, the `connected self` will be detected by
        // P2P framework, handling this situation by sending a `ConnectedSelfReq` message to
        // NodesManager. See the `handle` in `ConnectedSelfReq` for more detail.
        // This logic would be entry twice:
        // one as server, and the other one as client.
        let connect_self = handle_connect_self(self.session_id, &self.init_msg.peer_key, service);

        // Found a successful connection after exchanging `init message`.
        // FIXME: If have reached to max_connects, disconnected this node.
        // Add connected address.
        let mut node_cert: Option<X509> = None;
        if !repeated_connect && !connect_self {
            if service.enable_ca {
                if let Some(cert) = self.init_msg.node_crt {
                    let node_crt = X509::from_pem(cert.as_ref())
                        .expect("The client's  certificate file format error! Needs X509 format.");

                    if let Some(ref root_crt) = service.root_crt {
                        if let Err(e) = cita_verify_crt(
                            root_crt,
                            &node_crt,
                            &self.init_msg.peer_key,
                            &service.crl,
                        ) {
                            error!("Failed to verify certificate: {:?}.", e);
                            error!("Disconnet this session: {}", self.session_id);
                            if let Some(ref mut ctrl) = service.service_ctrl {
                                let _ = ctrl.disconnect(self.session_id);
                            }
                        } else {
                            node_cert = Some(node_crt);
                            info!("Cerificate verify OK");
                        }
                    }
                } else {
                    error!("The client do not contains a node certificate file in CA mode!");
                    error!("Disconnet this session: {}", self.session_id);
                    if let Some(ref mut ctrl) = service.service_ctrl {
                        let _ = ctrl.disconnect(self.session_id);
                    }
                }
            }

            if let Some(session_info) = service.pending_connected_addrs.remove(&self.session_id) {
                info!(
                    "[NodeManager] Add session [{:?}], address: {:?} to Connected_addrs.",
                    self.session_id, session_info.addr
                );
                // If the session have been writen in handle AddRepeatedNodeReq, just update the info.
                service
                    .connected_addrs
                    .entry(self.session_id)
                    .and_modify(|v| {
                        v.conn_addr = session_info.addr;
                        v.node_crt = node_cert.clone();
                    })
                    .or_insert_with(|| ConnectedInfo::new(session_info.addr, None, node_cert));

                // If it is an active connection, need to set this node in known_addrs has been connected.
                if self.ty == SessionType::Outbound {
                    if let Some(ref mut node_status) =
                        service.known_addrs.get_mut(&session_info.addr)
                    {
                        node_status.session_id = Some(self.session_id);
                        node_status.score += SUCCESS_DIALING_SCORE;
                    }
                }
            }

            // Add connected peer keys
            // Because AddRepeatedNodeReq maybe already did above action
            let _ = service
                .connected_peer_keys
                .insert(self.init_msg.peer_key, self.session_id);
            service
                .consensus_topology
                .add_linked_nodes(self.init_msg.peer_key);

            for (key, value) in service.connected_addrs.iter() {
                let crt_cn = if let Some(ref cert) = value.node_crt {
                    let subject = cert.subject_name();
                    let cn_str = str::from_utf8(
                        subject
                            .entries_by_nid(Nid::COMMONNAME)
                            .next()
                            .unwrap()
                            .data()
                            .as_slice(),
                    )
                    .unwrap();
                    Some(cn_str)
                } else {
                    None
                };

                info!(
                    "[NodeManager] connected_addrs info: [key: {:?}, connected address: {:?}, certificate common name: {:?}]",
                    key, value.conn_addr, crt_cn
                );
            }
            info!("[NodeManager] known_addrs info: {:?}", service.known_addrs);

            info!(
                "[NodeManager] Address in connected : {:?}",
                service.connected_peer_keys
            );
        }
        // End of dealing node for this round.
        if self.ty == SessionType::Outbound {
            service.dialing_node = None;
        }
    }
}

#[derive(Default)]
pub struct NetworkInitReq {
    session_id: SessionId,
}

impl NetworkInitReq {
    pub fn new(session_id: SessionId) -> Self {
        NetworkInitReq { session_id }
    }

    pub fn handle(self, service: &mut NodesManager) {
        let peer_key = service.peer_key;

        let mut init_msg = InitMsg {
            chain_id: 0,
            peer_key,
            node_crt: None,
        };

        if service.enable_ca {
            init_msg.node_crt = service.node_crt.clone();
        }
        let mut msg_unit = NetMessageUnit::default();
        msg_unit.key = "network.init".to_string();
        msg_unit.data = init_msg.into();

        if let Some(buf) = pubsub_message_to_network_message(&msg_unit) {
            if let Some(ref mut ctrl) = service.service_ctrl {
                let ret = ctrl.send_message_to(self.session_id, TRANSFER_PROTOCOL_ID, buf);
                info!(
                    "[NodeManager] Send network init message!, id: {:?}, peer_addr: {:?}, ret: {:?}",
                    self.session_id, peer_key, ret,
                );
            }
        }
    }
}

pub struct AddNodeReq {
    addr: SocketAddr,
    source: NodeSource,
}

impl AddNodeReq {
    pub fn new(addr: SocketAddr, source: NodeSource) -> Self {
        AddNodeReq { addr, source }
    }

    pub fn handle(self, service: &mut NodesManager) {
        if service.known_addrs.len() > DEFAULT_MAX_KNOWN_ADDRS {
            warn!(
                "[NodeManager] Known address has reach Max: {:?}",
                DEFAULT_MAX_KNOWN_ADDRS,
            );
            return;
        }
        // Add a new node, using a default node status.
        let default_node_status = NodeStatus::new(FULL_SCORE, None, self.source);
        service
            .known_addrs
            .entry(self.addr)
            .or_insert(default_node_status);
    }
}

pub struct DialedErrorReq {
    addr: SocketAddr,
}

impl DialedErrorReq {
    pub fn new(addr: SocketAddr) -> Self {
        DialedErrorReq { addr }
    }

    pub fn handle(self, service: &mut NodesManager) {
        if let Some(ref mut node_status) = service.known_addrs.get_mut(&self.addr) {
            node_status.score -= DIALED_ERROR_SCORE;
        }

        // Catch a dial error, this dialing finished
        service.dialing_node = None;
    }
}

pub struct AddRepeatedNodeReq {
    addr: SocketAddr,
    session_id: SessionId,
}

impl AddRepeatedNodeReq {
    pub fn new(addr: SocketAddr, session_id: SessionId) -> Self {
        AddRepeatedNodeReq { addr, session_id }
    }

    pub fn handle(self, service: &mut NodesManager) {
        info!(
            "[NodeManager] Dialing a repeated node [{:?}], on session: {:?}.",
            self.addr, self.session_id
        );

        if let Some(ref mut node_status) = service.known_addrs.get_mut(&self.addr) {
            node_status.session_id = Some(self.session_id);
            node_status.score += SUCCESS_DIALING_SCORE;

            // Just save the trans_addr for this session, other field will be writen in hanld AddConnectedNodeReq.
            service
                .connected_addrs
                .entry(self.session_id)
                .and_modify(|v| {
                    v.trans_addr = Some(self.addr);
                })
                .or_insert_with(|| {
                    ConnectedInfo::new(
                        SocketAddr::from_str("0.0.0.0:0").unwrap(),
                        Some(self.addr),
                        None,
                    )
                });
        } else {
            warn!("[NodeManager] Cant find repeated sock addr in known addrs");
        }
        // This dialing is finished.
        service.dialing_node = None;
    }
}

pub struct GetRandomNodesReq {
    num: usize,
    return_channel: Sender<Vec<SocketAddr>>,
}

impl GetRandomNodesReq {
    pub fn new(num: usize, return_channel: Sender<Vec<SocketAddr>>) -> Self {
        GetRandomNodesReq {
            num,
            return_channel,
        }
    }

    pub fn handle(self, service: &mut NodesManager) {
        let mut addrs: Vec<_> = service.known_addrs.keys().cloned().collect();
        thread_rng().shuffle(&mut addrs);
        addrs.truncate(self.num);

        if let Err(e) = self.return_channel.try_send(addrs) {
            warn!(
                "[NodeManager] Get random n nodes, send them failed : {:?}",
                e
            );
        }
    }
}

pub struct PendingConnectedNodeReq {
    session_id: SessionId,
    addr: SocketAddr,
    ty: SessionType,
}

impl PendingConnectedNodeReq {
    pub fn new(session_id: SessionId, addr: SocketAddr, ty: SessionType) -> Self {
        PendingConnectedNodeReq {
            session_id,
            addr,
            ty,
        }
    }

    pub fn handle(self, service: &mut NodesManager) {
        if service.connected_addrs.len() >= service.max_connects {
            // Has reached to max connects, refuse this connection
            info!(
                "[NodeManager] Has reached to max connects [{:?}], refuse Session [{:?}], address: {:?}",
                service.max_connects, self.session_id, self.addr
            );
            if let Some(ref mut ctrl) = service.service_ctrl {
                let _ = ctrl.disconnect(self.session_id);
            }
            return;
        }

        info!(
            "[NodeManager] Session [{:?}], address: {:?} pending to add to Connected_addrs.",
            self.session_id, self.addr
        );
        service
            .pending_connected_addrs
            .insert(self.session_id, SessionInfo::new(self.ty, self.addr));
    }
}

pub struct DelConnectedNodeReq {
    session_id: SessionId,
}

impl DelConnectedNodeReq {
    pub fn new(session_id: SessionId) -> Self {
        DelConnectedNodeReq { session_id }
    }

    pub fn handle(self, service: &mut NodesManager) {
        info!("[NodeManager] Disconnected session [{:?}]", self.session_id);

        if let Some(addr) = service.connected_addrs.remove(&self.session_id) {
            let trans_addr = addr.trans_addr.unwrap_or(addr.conn_addr);
            self.fix_node_status(trans_addr, service);

            // Remove connected peer keys
            let key = {
                if let Some((&key, _)) = service
                    .connected_peer_keys
                    .iter()
                    .find(|(_, &v)| v == self.session_id)
                {
                    Some(key)
                } else {
                    None
                }
            };

            if let Some(key) = key {
                service.consensus_topology.del_linked_nodes(&key);
                service.connected_peer_keys.remove(&key);
            }
        }

        // Remove pending connected
        if let Some(session_info) = service.pending_connected_addrs.remove(&self.session_id) {
            if session_info.ty == SessionType::Outbound {
                self.fix_node_status(session_info.addr, service);
                // Close a session which open as client, end of this dialing.
                service.dialing_node = None;
            }
        }
    }

    fn fix_node_status(&self, addr: SocketAddr, service: &mut NodesManager) {
        // Set the node as disconnected in known_addrs
        if let Some(ref mut node_status) = service.known_addrs.get_mut(&addr) {
            if let Some(session_id) = node_status.session_id {
                if session_id == self.session_id {
                    info!("Reset node status of address {:?} to None", addr);
                    node_status.score -= REFUSED_SCORE;
                    node_status.session_id = None;
                } else {
                    warn!(
                        "[NodeManager] Expected session id: {:?}, but found: {:?}",
                        self.session_id, session_id
                    );
                }
            } else {
                error!("[NodeManager] Can not get node status from known_addr, this should not happen!");
            }
        }
    }
}

#[derive(Debug)]
pub struct RetransNetMsgReq {
    msg_unit: NetMessageUnit,
    incomming_session_id: SessionId,
}

impl RetransNetMsgReq {
    pub fn new(msg_unit: NetMessageUnit, incomming_session_id: SessionId) -> Self {
        RetransNetMsgReq {
            msg_unit,
            incomming_session_id,
        }
    }

    pub fn handle(mut self, service: &mut NodesManager) {
        let msg_version = self.msg_unit.version;
        let in_id = self.incomming_session_id;

        trace!(
            "[NodeManager] RetranseReq msg.key {:?}, from session {},version {} self current version {} ttl {}",
            self.msg_unit.key,
            self.incomming_session_id,
            msg_version,
            service.self_version,
            self.msg_unit.ttl,
        );

        let saved_version = service
            .gossip_key_version
            .entry(self.msg_unit.addr)
            .or_insert(0);
        if msg_version == 0 || *saved_version < msg_version {
            *saved_version = msg_version;
            let mut ids: Vec<_> = service.connected_addrs.keys().cloned().collect();
            ids.retain(|id| *id != in_id);

            if service.consensus_topology.consensus_all_linked {
                self.msg_unit.ttl = 0;
            }

            if let Some(buf) = pubsub_message_to_network_message(&self.msg_unit) {
                if let Some(ref mut ctrl) = service.service_ctrl {
                    let _ =
                        ctrl.filter_broadcast(TargetSession::Multi(ids), TRANSFER_PROTOCOL_ID, buf);
                }
            }
        }
    }
}

// Call the system contract "CERT_REVOKE_MANAGER" to getCrl.
fn get_crl(service: &mut NodesManager) {
    let mut request = create_request();
    let mut call = Call::new();

    let get_crl_hash: Vec<u8> = encode_to_vec(b"getCrl()");
    // Todo: use CERT_REVOKE_MANAGER address in cita-type
    let contract_address: Address =
        Address::from_str("ffffffffffffffffffffffffffffffffff020030").unwrap();
    call.set_from(Address::default().to_vec());
    call.set_to(contract_address.to_vec());
    call.set_data(get_crl_hash);
    call.set_height(("\"latest\"").to_owned());
    request.set_call(call);

    let data: Message = request.into();
    let msg = PubMessage::new(routing_key!(Net >> GetCrl).into(), data.try_into().unwrap());
    service.mq_client.get_crl(msg);
}

#[derive(Debug)]
pub struct DealRichStatusReq {
    msg: ProtoMessage,
}

impl DealRichStatusReq {
    pub fn new(msg: ProtoMessage) -> Self {
        DealRichStatusReq { msg }
    }

    pub fn handle(mut self, service: &mut NodesManager) {
        let rich_status = self.msg.take_rich_status().unwrap();
        info!("DealRichStatusReq rich status {:?}", rich_status);

        if service.enable_ca {
            let current_hight = rich_status.get_height();
            // service.get_crl_point init as 0, if current_hight > UPDATE_CRL_PERIOD, network will get the CRL for its each reset.
            if current_hight - service.get_crl_point > UPDATE_CRL_PERIOD {
                info!("Get Certificate Revoke List from Executor!");
                get_crl(service);
                service.get_crl_point = current_hight;
            }
        }

        let validators: BTreeSet<Address> = rich_status
            .get_validators()
            .iter()
            .map(|node| Address::from_slice(node))
            .collect();

        service
            .consensus_topology
            .update_validators(rich_status.get_height(), validators);
    }
}

#[derive(Debug)]
pub struct BroadcastReq {
    key: String,
    msg: ProtoMessage,
}

impl BroadcastReq {
    pub fn new(key: String, msg: ProtoMessage) -> Self {
        BroadcastReq { key, msg }
    }

    pub fn handle(self, service: &mut NodesManager) {
        trace!(
            "[NodeManager] Broadcast msg {:?}, from key {}",
            self.msg,
            self.key
        );

        let mut info = NetMessageUnit::default();
        info.key = self.key;
        info.data = self.msg.try_into().unwrap();
        info.addr = service.peer_key;
        info.version = service.self_version;
        service.self_version += 1;

        // Broadcast msg with three types:
        // Synchronizer >> Status for declaring myself status,only send to neighbors
        // If consensus node all be connected,consensus msg and tx msg only be sent once
        // No need to resend tx info
        if !service.consensus_topology.consensus_all_linked() && info.key.contains(CONSENSUS_STR) {
            info.ttl = CONSENSUS_TTL_NUM;
        }

        if let Some(buf) = pubsub_message_to_network_message(&info) {
            if let Some(ref mut ctrl) = service.service_ctrl {
                let _ = ctrl.filter_broadcast(TargetSession::All, TRANSFER_PROTOCOL_ID, buf);
            }
        }
    }
}

pub struct SingleTxReq {
    dst: SessionId,
    key: String,
    msg: ProtoMessage,
}

impl SingleTxReq {
    pub fn new(dst: SessionId, key: String, msg: ProtoMessage) -> Self {
        SingleTxReq { dst, key, msg }
    }

    pub fn handle(self, service: &mut NodesManager) {
        trace!(
            "[NodeManager] Send msg {:?} to {}, from key {}",
            self.msg,
            self.dst,
            self.key
        );
        let dst = self.dst;
        let mut msg_unit = NetMessageUnit::default();
        msg_unit.key = self.key;
        msg_unit.data = self.msg.try_into().unwrap();

        if let Some(buf) = pubsub_message_to_network_message(&msg_unit) {
            if let Some(ref mut ctrl) = service.service_ctrl {
                let _ = ctrl.send_message_to(dst, TRANSFER_PROTOCOL_ID, buf);
            }
        }
    }
}

pub struct GetPeerCountReq {
    return_channel: Sender<usize>,
}

impl GetPeerCountReq {
    pub fn new(return_channel: Sender<usize>) -> Self {
        GetPeerCountReq { return_channel }
    }

    pub fn handle(self, service: &mut NodesManager) {
        let peer_count = service.connected_addrs.len();

        if let Err(e) = self.return_channel.try_send(peer_count) {
            warn!(
                "[NodeManager] Get peer count {}, but send it failed : {:?}",
                peer_count, e
            );
        }
    }
}

pub struct GetPeersInfoReq {
    return_channel: Sender<HashMap<Address, String>>,
}

impl GetPeersInfoReq {
    pub fn new(return_channel: Sender<HashMap<Address, String>>) -> Self {
        GetPeersInfoReq { return_channel }
    }

    pub fn handle(self, service: &mut NodesManager) {
        let mut peers = HashMap::default();

        for (key, value) in service.connected_peer_keys.iter() {
            if let Some(addr) = service.connected_addrs.get(&value) {
                peers.insert(key.clone(), addr.conn_addr.ip().to_string());
            } else {
                warn!(
                    "[NodeManager] Can not get socket address for session {} from connected_addr. It must be something wrong!",
                    value
                );
            }
        }

        debug!("[NodeManager] get peers info : {:?}", peers);

        if let Err(e) = self.return_channel.try_send(peers) {
            warn!("[NodeManager] Send peers info failed : {:?}", e);
        }
    }
}

pub struct ConnectedSelfReq {
    addr: SocketAddr,
}

impl ConnectedSelfReq {
    pub fn new(addr: SocketAddr) -> Self {
        ConnectedSelfReq { addr }
    }

    pub fn handle(self, service: &mut NodesManager) {
        service.self_addr = Some(self.addr);
        service.dialing_node = None;
    }
}

pub struct ModifiedConfigPeersReq {
    peers: Vec<String>,
}

impl ModifiedConfigPeersReq {
    pub fn new(peers: Vec<String>) -> Self {
        ModifiedConfigPeersReq { peers }
    }

    pub fn handle(self, service: &mut NodesManager) {
        // If new config deleted some peer,disconnect and remove it from known addrs
        let mut keys: BTreeSet<_> = service.config_addrs.keys().cloned().collect();
        for peer in &self.peers {
            keys.remove(peer);
        }

        info!("left peers {:?}", self.peers);

        // The remainder in keys will be disconnected
        for key in keys {
            service.config_addrs.remove(&key).and_then(|addr| {
                addr.and_then(|addr| {
                    service.known_addrs.remove(&addr).and_then(|node_status| {
                        node_status.session_id.and_then(|sid| {
                            service
                                .service_ctrl
                                .as_mut()
                                .and_then(|ctrl| ctrl.disconnect(sid).ok())
                        })
                    })
                })
            });
        }
        for peer in self.peers {
            service.config_addrs.entry(peer).or_insert(None);
        }
    }
}

pub struct UpdateCrlReq {
    crl: Vec<Address>,
}

impl UpdateCrlReq {
    pub fn new(crl: Vec<Address>) -> Self {
        UpdateCrlReq { crl }
    }

    pub fn handle(self, service: &mut NodesManager) {
        // update crl in NodesManager
        info!("Update Certificate Revoke List as : {:?}", self.crl);
        service.crl = self.crl;
        for addr in service.crl.iter() {
            for (key, value) in service.connected_addrs.iter() {
                if let Some(ref crt) = value.node_crt {
                    // It can can definitely get the address from crt. So it is ok to use unwrap here.
                    let cn_addr = get_common_name(crt).unwrap();

                    // Find a connecting node in CRL, disconnect it!
                    if addr.contains(&cn_addr) {
                        info!("Find connecting node {:?} in CRL, disconnect it!", addr);
                        if let Some(ref mut ctrl) = service.service_ctrl {
                            let _ = ctrl.disconnect(*key);
                        }
                    }
                }
            }
        }
    }
}
