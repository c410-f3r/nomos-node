// std
use std::net::SocketAddr;
use std::ops::Range;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::Duration;
// crates
use blst::min_sig::SecretKey;
use cl::{InputWitness, NoteWitness, NullifierSecret};
use cryptarchia_consensus::{CryptarchiaInfo, CryptarchiaSettings, TimeConfig};
use cryptarchia_ledger::LedgerState;
use kzgrs_backend::dispersal::BlobInfo;
#[cfg(feature = "mixnet")]
use mixnet::{
    address::NodeAddress,
    client::MixClientConfig,
    node::MixNodeConfig,
    topology::{MixNodeInfo, MixnetTopology},
};
use nomos_core::{block::Block, header::HeaderId, staking::NMO_UNIT};
use nomos_da_indexer::storage::adapters::rocksdb::RocksAdapterSettings as IndexerStorageAdapterSettings;
use nomos_da_indexer::IndexerSettings;
use nomos_da_network_service::backends::libp2p::common::DaNetworkBackendSettings;
use nomos_da_network_service::NetworkConfig as DaNetworkConfig;
use nomos_da_sampling::backend::kzgrs::KzgrsSamplingBackendSettings;
use nomos_da_sampling::storage::adapters::rocksdb::RocksAdapterSettings as SamplingStorageAdapterSettings;
use nomos_da_sampling::DaSamplingServiceSettings;
use nomos_da_verifier::backend::kzgrs::KzgrsDaVerifierSettings;
use nomos_da_verifier::storage::adapters::rocksdb::RocksAdapterSettings as VerifierStorageAdapterSettings;
use nomos_da_verifier::DaVerifierServiceSettings;
use nomos_libp2p::{Multiaddr, PeerId, SwarmConfig};
use nomos_log::{LoggerBackend, LoggerFormat};
use nomos_mempool::MempoolMetrics;
#[cfg(feature = "mixnet")]
use nomos_network::backends::libp2p::mixnet::MixnetConfig;
use nomos_network::{backends::libp2p::Libp2pConfig, NetworkConfig};
use nomos_node::{api::AxumBackendSettings, Config, Tx};
use nomos_storage::backends::rocksdb::RocksBackendSettings;
use once_cell::sync::Lazy;
use rand::{thread_rng, Rng};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use subnetworks_assignations::versions::v1::FillFromNodeList;
use subnetworks_assignations::MembershipHandler;
use tempfile::NamedTempFile;
use time::OffsetDateTime;
// internal
use super::{create_tempdir, persist_tempdir, LOGS_PREFIX};
use crate::{adjust_timeout, get_available_port, ConsensusConfig, DaConfig, Node, TestConfig};

static CLIENT: Lazy<Client> = Lazy::new(Client::new);
const CRYPTARCHIA_INFO_API: &str = "cryptarchia/info";
const GET_HEADERS_INFO: &str = "cryptarchia/headers";
const NOMOS_BIN: &str = "../target/debug/nomos-node";
const STORAGE_BLOCKS_API: &str = "storage/block";
const INDEXER_RANGE_API: &str = "da/get_range";
const DEFAULT_SLOT_TIME: u64 = 2;
const CONSENSUS_SLOT_TIME_VAR: &str = "CONSENSUS_SLOT_TIME";
#[cfg(feature = "mixnet")]
const NUM_MIXNODE_CANDIDATES: usize = 2;

pub struct NomosNode {
    addr: SocketAddr,
    _tempdir: tempfile::TempDir,
    child: Child,
    config: Config,
}

impl Drop for NomosNode {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Err(e) = persist_tempdir(&mut self._tempdir, "nomos-node") {
                println!("failed to persist tempdir: {e}");
            }
        }

        if let Err(e) = self.child.kill() {
            println!("failed to kill the child process: {e}");
        }
    }
}
impl NomosNode {
    pub async fn spawn_inner(mut config: Config) -> Self {
        // Waku stores the messages in a db file in the current dir, we need a different
        // directory for each node to avoid conflicts
        let dir = create_tempdir().unwrap();
        let mut file = NamedTempFile::new().unwrap();
        let config_path = file.path().to_owned();
        let wait_online_secs = config.wait_online_secs;

        // setup logging so that we can intercept it later in testing
        config.log.backend = LoggerBackend::File {
            directory: dir.path().to_owned(),
            prefix: Some(LOGS_PREFIX.into()),
        };
        config.log.format = LoggerFormat::Json;

        config.storage.db_path = dir.path().join("db");
        config
            .da_sampling
            .storage_adapter_settings
            .blob_storage_directory = dir.path().to_owned();
        config
            .da_verifier
            .storage_adapter_settings
            .blob_storage_directory = dir.path().to_owned();
        config.da_indexer.storage.blob_storage_directory = dir.path().to_owned();

        serde_yaml::to_writer(&mut file, &config).unwrap();
        let child = Command::new(std::env::current_dir().unwrap().join(NOMOS_BIN))
            .arg(&config_path)
            .current_dir(dir.path())
            .stdout(Stdio::inherit())
            .spawn()
            .unwrap();
        let node = Self {
            addr: config.http.backend_settings.address,
            child,
            _tempdir: dir,
            config,
        };
        tokio::time::timeout(
            adjust_timeout(Duration::from_secs(wait_online_secs)),
            async { node.wait_online().await },
        )
        .await
        .unwrap();

        node
    }

    async fn get(&self, path: &str) -> reqwest::Result<reqwest::Response> {
        CLIENT
            .get(format!("http://{}/{}", self.addr, path))
            .send()
            .await
    }

    pub fn url(&self) -> Url {
        format!("http://{}", self.addr).parse().unwrap()
    }

    async fn wait_online(&self) {
        loop {
            let res = self.get("cl/metrics").await;
            if res.is_ok() && res.unwrap().status().is_success() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn get_block(&self, id: HeaderId) -> Option<Block<Tx, BlobInfo>> {
        CLIENT
            .post(format!("http://{}/{}", self.addr, STORAGE_BLOCKS_API))
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&id).unwrap())
            .send()
            .await
            .unwrap()
            .json::<Option<Block<Tx, BlobInfo>>>()
            .await
            .unwrap()
    }

    pub async fn get_mempoool_metrics(&self, pool: Pool) -> MempoolMetrics {
        let discr = match pool {
            Pool::Cl => "cl",
            Pool::Da => "da",
        };
        let addr = format!("{}/metrics", discr);
        let res = self
            .get(&addr)
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        MempoolMetrics {
            pending_items: res["pending_items"].as_u64().unwrap() as usize,
            last_item_timestamp: res["last_item_timestamp"].as_u64().unwrap(),
        }
    }

    pub async fn get_indexer_range(
        &self,
        app_id: [u8; 32],
        range: Range<[u8; 8]>,
    ) -> Vec<([u8; 8], Vec<Vec<u8>>)> {
        CLIENT
            .post(format!("http://{}/{}", self.addr, INDEXER_RANGE_API))
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&GetRangeReq { app_id, range }).unwrap())
            .send()
            .await
            .unwrap()
            .json::<Vec<([u8; 8], Vec<Vec<u8>>)>>()
            .await
            .unwrap()
    }

    // not async so that we can use this in `Drop`
    pub fn get_logs_from_file(&self) -> String {
        println!(
            "fetching logs from dir {}...",
            self._tempdir.path().display()
        );
        // std::thread::sleep(std::time::Duration::from_secs(50));
        std::fs::read_dir(self._tempdir.path())
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_file() && path.to_str().unwrap().contains(LOGS_PREFIX) {
                    Some(path)
                } else {
                    None
                }
            })
            .map(|f| std::fs::read_to_string(f).unwrap())
            .collect::<String>()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub async fn get_headers(&self, from: Option<HeaderId>, to: Option<HeaderId>) -> Vec<HeaderId> {
        let mut req = CLIENT.get(format!("http://{}/{}", self.addr, GET_HEADERS_INFO));

        if let Some(from) = from {
            req = req.query(&[("from", from)]);
        }

        if let Some(to) = to {
            req = req.query(&[("to", to)]);
        }

        let res = req.send().await;

        println!("res: {res:?}");

        res.unwrap().json::<Vec<HeaderId>>().await.unwrap()
    }
}

#[async_trait::async_trait]
impl Node for NomosNode {
    type ConsensusInfo = CryptarchiaInfo;

    async fn spawn(config: Config) -> Self {
        Self::spawn_inner(config).await
    }

    async fn consensus_info(&self) -> Self::ConsensusInfo {
        let res = self.get(CRYPTARCHIA_INFO_API).await;
        println!("{:?}", res);
        res.unwrap().json().await.unwrap()
    }

    fn stop(&mut self) {
        self.child.kill().unwrap();
    }

    /// Depending on the network topology, the next leader must be spawned first,
    /// so the leader can receive votes from all other nodes that will be subsequently spawned.
    /// If not, the leader will miss votes from nodes spawned before itself.
    /// This issue will be resolved by devising the block catch-up mechanism in the future.
    fn create_node_configs(
        consensus: ConsensusConfig,
        da: DaConfig,
        test: TestConfig,
    ) -> Vec<Config> {
        // we use the same random bytes for:
        // * da id
        // * coin sk
        // * coin nonce
        let mut ids = vec![[0; 32]; consensus.n_participants];
        for id in &mut ids {
            thread_rng().fill(id);
        }

        #[cfg(feature = "mixnet")]
        let (mixclient_config, mixnode_configs) = create_mixnet_config(&ids);

        let notes = ids
            .iter()
            .map(|&id| {
                let mut sk = [0; 16];
                sk.copy_from_slice(&id[0..16]);
                InputWitness::new(
                    NoteWitness::basic(1, NMO_UNIT, &mut thread_rng()),
                    NullifierSecret(sk),
                )
            })
            .collect::<Vec<_>>();
        // no commitments for now, proofs are not checked anyway
        let genesis_state = LedgerState::from_commitments(
            notes.iter().map(|n| n.note_commitment()),
            (ids.len() as u32).into(),
        );
        let ledger_config = cryptarchia_ledger::Config {
            epoch_stake_distribution_stabilization: 3,
            epoch_period_nonce_buffer: 3,
            epoch_period_nonce_stabilization: 4,
            consensus_config: cryptarchia_engine::Config {
                security_param: consensus.security_param,
                active_slot_coeff: consensus.active_slot_coeff,
            },
        };
        let slot_duration = std::env::var(CONSENSUS_SLOT_TIME_VAR)
            .map(|s| <u64>::from_str(&s).unwrap())
            .unwrap_or(DEFAULT_SLOT_TIME);
        let time_config = TimeConfig {
            slot_duration: Duration::from_secs(slot_duration),
            chain_start_time: OffsetDateTime::now_utc(),
        };

        #[allow(unused_mut, unused_variables)]
        let mut configs = ids
            .into_iter()
            .zip(notes)
            .enumerate()
            .map(|(i, (da_id, coin))| {
                create_node_config(
                    da_id,
                    genesis_state.clone(),
                    ledger_config.clone(),
                    vec![coin],
                    time_config.clone(),
                    da.clone(),
                    test.wait_online_secs,
                    #[cfg(feature = "mixnet")]
                    MixnetConfig {
                        mixclient: mixclient_config.clone(),
                        mixnode: mixnode_configs[i].clone(),
                    },
                )
            })
            .collect::<Vec<_>>();

        // Build DA memberships and address lists.
        let peer_addresses = build_da_peer_list(&configs);
        let mut peer_ids = peer_addresses.iter().map(|(p, _)| *p).collect::<Vec<_>>();
        peer_ids.extend(da.executor_peer_ids);

        for config in &mut configs {
            let membership =
                FillFromNodeList::new(&peer_ids, da.subnetwork_size, da.dispersal_factor);
            let local_peer_id = secret_key_to_peer_id(config.da_network.backend.node_key.clone());
            let subnetwork_ids = membership.membership(&local_peer_id);
            config.da_verifier.verifier_settings.index = subnetwork_ids;
            config.da_network.backend.membership = membership;
            config.da_network.backend.addresses = peer_addresses.iter().cloned().collect();
        }

        #[cfg(feature = "mixnet")]
        {
            // Build a topology using only a subset of nodes.
            let mixnode_candidates = configs
                .iter()
                .take(NUM_MIXNODE_CANDIDATES)
                .collect::<Vec<_>>();
            let topology = build_mixnet_topology(&mixnode_candidates);

            // Set the topology to all configs
            for config in &mut configs {
                config.network.backend.mixnet.mixclient.topology = topology.clone();
            }
            configs
        }
        #[cfg(not(feature = "mixnet"))]
        configs
    }
}

pub enum Pool {
    Da,
    Cl,
}

#[derive(Serialize, Deserialize)]
struct GetRangeReq {
    pub app_id: [u8; 32],
    pub range: Range<[u8; 8]>,
}

#[cfg(feature = "mixnet")]
fn create_mixnet_config(ids: &[[u8; 32]]) -> (MixClientConfig, Vec<MixNodeConfig>) {
    use std::num::NonZeroU8;

    let mixnode_configs: Vec<MixNodeConfig> = ids
        .iter()
        .map(|id| MixNodeConfig {
            encryption_private_key: *id,
            delay_rate_per_min: 100000000.0,
        })
        .collect();
    // Build an empty topology because it will be constructed with meaningful node infos later
    let topology = MixnetTopology::new(Vec::new(), 0, 0, [1u8; 32]).unwrap();

    (
        MixClientConfig {
            topology,
            emission_rate_per_min: 120.0,
            redundancy: NonZeroU8::new(1).unwrap(),
        },
        mixnode_configs,
    )
}

#[cfg(feature = "mixnet")]
fn build_mixnet_topology(mixnode_candidates: &[&Config]) -> MixnetTopology {
    use mixnet::crypto::public_key_from;
    use std::net::{IpAddr, Ipv4Addr};

    let candidates = mixnode_candidates
        .iter()
        .map(|config| {
            MixNodeInfo::new(
                NodeAddress::from(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                    config.network.backend.inner.port,
                )),
                public_key_from(config.network.backend.mixnet.mixnode.encryption_private_key),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let num_layers = candidates.len();
    MixnetTopology::new(candidates, num_layers, 1, [1u8; 32]).unwrap()
}

fn secret_key_to_peer_id(node_key: nomos_libp2p::ed25519::SecretKey) -> PeerId {
    PeerId::from_public_key(
        &nomos_libp2p::ed25519::Keypair::from(node_key)
            .public()
            .into(),
    )
}

fn build_da_peer_list(configs: &[Config]) -> Vec<(PeerId, Multiaddr)> {
    configs
        .iter()
        .map(|c| {
            (
                secret_key_to_peer_id(c.da_network.backend.node_key.clone()),
                c.da_network.backend.listening_address.clone(),
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn create_node_config(
    id: [u8; 32],
    genesis_state: LedgerState,
    config: cryptarchia_ledger::Config,
    notes: Vec<InputWitness>,
    time: TimeConfig,
    da_config: DaConfig,
    wait_online_secs: u64,
    #[cfg(feature = "mixnet")] mixnet_config: MixnetConfig,
) -> Config {
    let swarm_config: SwarmConfig = Default::default();

    let verifier_sk = SecretKey::key_gen(&id, &[]).unwrap();
    let verifier_sk_bytes = verifier_sk.to_bytes();

    let mut config = Config {
        network: NetworkConfig {
            backend: Libp2pConfig {
                inner: swarm_config.clone(),
                initial_peers: vec![],
                #[cfg(feature = "mixnet")]
                mixnet: mixnet_config,
            },
        },
        cryptarchia: CryptarchiaSettings {
            notes,
            config,
            genesis_state,
            time,
            transaction_selector_settings: (),
            blob_selector_settings: (),
        },
        da_network: DaNetworkConfig {
            backend: DaNetworkBackendSettings {
                node_key: swarm_config.node_key,
                listening_address: Multiaddr::from_str(&format!(
                    "/ip4/127.0.0.1/udp/{}/quic-v1",
                    get_available_port(),
                ))
                .unwrap(),
                addresses: Default::default(),
                membership: Default::default(),
            },
        },
        da_indexer: IndexerSettings {
            storage: IndexerStorageAdapterSettings {
                blob_storage_directory: "./".into(),
            },
        },
        da_verifier: DaVerifierServiceSettings {
            verifier_settings: KzgrsDaVerifierSettings {
                sk: hex::encode(verifier_sk_bytes),
                index: Default::default(),
                global_params_path: da_config.global_params_path,
            },
            network_adapter_settings: (),
            storage_adapter_settings: VerifierStorageAdapterSettings {
                blob_storage_directory: "./".into(),
            },
        },
        log: Default::default(),
        http: nomos_api::ApiServiceSettings {
            backend_settings: AxumBackendSettings {
                address: format!("127.0.0.1:{}", get_available_port())
                    .parse()
                    .unwrap(),
                cors_origins: vec![],
            },
        },
        da_sampling: DaSamplingServiceSettings {
            sampling_settings: KzgrsSamplingBackendSettings {
                num_samples: da_config.num_samples,
                num_subnets: da_config.num_subnets,
                old_blobs_check_interval: da_config.old_blobs_check_interval,
                blobs_validity_duration: da_config.blobs_validity_duration,
            },
            storage_adapter_settings: SamplingStorageAdapterSettings {
                blob_storage_directory: "./".into(),
            },
            network_adapter_settings: (),
        },
        storage: RocksBackendSettings {
            db_path: "./db".into(),
            read_only: false,
            column_family: Some("blocks".into()),
        },
        wait_online_secs,
    };

    config.network.backend.inner.port = get_available_port();

    config
}
