#![feature(once_cell)]

use env_logger::Env;
use log::info;
use std::collections::HashMap;

use std::{
    hash::{Hash, Hasher},
    collections::hash_map::DefaultHasher};
use std::ffi::CString;
use std::fs;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use std::{fs::File, io::Read};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Identity;
use tonic::transport::ServerTlsConfig;

use ring::digest;

use tonic::{transport::Server, Request, Response, Status, Streaming};
use uuid::Uuid;

mod remote_torch {
    tonic::include_proto!("remote_torch");
}
use remote_torch::remote_torch_server::{RemoteTorch, RemoteTorchServer};
use remote_torch::{
    Chunk, ClientInfo, Devices, Empty, Metric, Optimizers, Reference, References, TestConfig,
    TrainConfig,
};

mod telemetry;
use telemetry::TelemetryEventProps;

mod storage;
use storage::{Artifact, Dataset, Module};

mod utils;
use utils::*;

use crate::storage::SizedObjectsBytes;

struct BastionAIServer {
    modules: RwLock<HashMap<Uuid, Artifact<Module>>>,
    modules_hashes: RwLock<HashMap<Uuid, Arc<(String, Option<ClientInfo>)>>>,
    datasets: RwLock<HashMap<Uuid, Artifact<Dataset>>>,
    datasets_hashes: RwLock<HashMap<Uuid, Arc<(String, Option<ClientInfo>)>>>,

}

impl BastionAIServer {
    pub fn new() -> Self {
        BastionAIServer {
            modules: RwLock::new(HashMap::new()),
            modules_hashes: RwLock::new(HashMap::new()),
            datasets: RwLock::new(HashMap::new()),
            datasets_hashes: RwLock::new(HashMap::new())
        }
    }
}

#[tonic::async_trait]
impl RemoteTorch for BastionAIServer {
    type FetchDatasetStream = ReceiverStream<Result<Chunk, Status>>;
    type FetchModuleStream = ReceiverStream<Result<Chunk, Status>>;
    type TrainStream = ReceiverStream<Result<Metric, Status>>;
    type TestStream = ReceiverStream<Result<Metric, Status>>;

    async fn send_dataset(
        &self,
        request: Request<Streaming<Chunk>>,
    ) -> Result<Response<Reference>, Status> {
        let start_time = Instant::now();

        let (artifact, client_info, dataset_name, _): (
            Artifact<SizedObjectsBytes>,
            Option<ClientInfo>,
            String,
            _,
        ) = unstream_data(request.into_inner()).await?;

        let (dataset_hash, dataset_size) = {
            let lock = artifact.data.read().unwrap();
            let data = lock.get();
            let hash = hex::encode(digest::digest(&digest::SHA256, &data).as_ref());
            (hash, data.len())
        };

        let dataset: Artifact<Dataset> = tcherror_to_status((artifact).deserialize())?;
        let description = String::from(dataset.description.clone());
        let identifier = Uuid::new_v4();

        self.datasets
            .write()
            .unwrap()
            .insert(identifier.clone(), dataset);

            self.datasets_hashes.write().unwrap().insert(
                identifier.clone(),
                Arc::new((dataset_hash.clone(), client_info.clone())),
            );
        let elapsed = start_time.elapsed();
        info!(
        target: "BastionAI",
            
            "Upload Dataset successful in {}ms", elapsed.as_millis());

        telemetry::add_event(
            TelemetryEventProps::SendDataset {
                dataset_name: Some(dataset_name),
                dataset_size,
                time_taken: elapsed.as_millis() as f64,
                dataset_hash: Some(dataset_hash.clone())
            },
            client_info,
        );
        Ok(Response::new(Reference {
            identifier: format!("{}", identifier),
            description: description.clone(),
        }))
    }

    async fn send_model(
        &self,
        request: Request<Streaming<Chunk>>,
    ) -> Result<Response<Reference>, Status> {
        let start_time = Instant::now();

        let (artifact, client_info, _, model_name): (
            Artifact<SizedObjectsBytes>,
            Option<ClientInfo>,
            _,
            String,
        ) = unstream_data(request.into_inner()).await?;

        let (model_hash, model_size) = {
            let lock = artifact.data.read().unwrap();
            let data = lock.get();
            let hash = hex::encode(digest::digest(&digest::SHA256, &data).as_ref());
            (hash, data.len())
        };

        let module: Artifact<Module> = tcherror_to_status(artifact.deserialize())?;

        let description = String::from(module.description.clone());
        let identifier = Uuid::new_v4();

        self.modules
            .write()
            .unwrap()
            .insert(identifier.clone(), module);
        let elapsed = start_time.elapsed();

        self.modules_hashes.write().unwrap().insert(
            identifier.clone(),
            Arc::new((model_hash.clone(), client_info.clone())),
        );

        info!(
        target: "BastionAI",
            
            "Upload Model successful in {}ms", elapsed.as_millis());

        telemetry::add_event(
            TelemetryEventProps::SendModel {
                model_name: Some(model_name),
                model_hash: Some(model_hash),
                model_size,
                time_taken: elapsed.as_millis() as f64,
            },
            client_info,
        );
        Ok(Response::new(Reference {
            identifier: format!("{}", identifier),
            description,
        }))
    }

    async fn fetch_dataset(
        &self,
        request: Request<Reference>,
    ) -> Result<Response<Self::FetchDatasetStream>, Status> {
        let identifier = parse_reference(request.into_inner())?;
        let serialized = {
            let datasets = self.datasets.read().unwrap();
            let artifact = datasets
                .get(&identifier)
                .ok_or(Status::not_found("Not found"))?;
            tcherror_to_status(artifact.serialize())?
        };

        Ok(stream_data(serialized, 100_000_000, "Dataset".to_string()).await)
    }

    async fn fetch_module(
        &self,
        request: Request<Reference>,
    ) -> Result<Response<Self::FetchModuleStream>, Status> {
        let identifier = parse_reference(request.into_inner())?;
        let serialized = {
            let modules = self.modules.read().unwrap();
            let artifact = modules
                .get(&identifier)
                .ok_or(Status::not_found("Not found"))?;
            tcherror_to_status(artifact.serialize())?
        };

        Ok(stream_data(serialized, 100_000_000, "Model".to_string()).await)
    }

    async fn delete_dataset(&self, request: Request<Reference>) -> Result<Response<Empty>, Status> {
        let identifier = parse_reference(request.into_inner())?;
        self.datasets.write().unwrap().remove(&identifier);
        Ok(Response::new(Empty {}))
    }

    async fn delete_module(&self, request: Request<Reference>) -> Result<Response<Empty>, Status> {
        let identifier = parse_reference(request.into_inner())?;
        self.modules.write().unwrap().remove(&identifier);
        Ok(Response::new(Empty {}))
    }

    async fn train(
        &self,
        request: Request<TrainConfig>,
    ) -> Result<Response<Self::TrainStream>, Status> {
        let config = request.into_inner();
        let dataset_id = parse_reference(
            config
                .dataset
                .clone()
                .ok_or(Status::invalid_argument("Not found"))?,
        )?;
        let module_id = parse_reference(
            config
                .model
                .clone()
                .ok_or(Status::invalid_argument("Not found"))?,
        )?;
        let device = parse_device(&config.device)?;
        let module = {
            let modules = self.modules.read().unwrap();
            let module = modules
                .get(&module_id)
                .ok_or(Status::not_found("Not found"))?;
            Arc::clone(&module.data)
        };
        let model_client_info = {
            let modules_hashes = self.modules_hashes.read().unwrap();

            let model_hash = modules_hashes
                .get(&module_id)
                .ok_or(Status::not_found("Not found"))?;

            Arc::clone(&model_hash)
        };

        let dataset_client_info = {
            let datasets_hashes = self.datasets_hashes.read().unwrap();

            let dataset_hash = datasets_hashes
            .get(&dataset_id) 
            .ok_or(Status::not_found("Not found"))?;
            Arc::clone(&dataset_hash)
        };
        let dataset = {
            let datasets = self.datasets.read().unwrap();
            let dataset = datasets
                .get(&dataset_id)
                .ok_or(Status::not_found("Not found"))?;
            Arc::clone(&dataset.data)
        };
        Ok(stream_module_train(module, dataset, config, device, model_client_info, dataset_client_info).await)
    }

    async fn test(
        &self,
        request: Request<TestConfig>,
    ) -> Result<Response<Self::TestStream>, Status> {
        let config = request.into_inner();
        let dataset_id = parse_reference(
            config
                .dataset
                .clone()
                .ok_or(Status::invalid_argument("Not found"))?,
        )?;
        let module_id = parse_reference(
            config
                .model
                .clone()
                .ok_or(Status::invalid_argument("Not found"))?,
        )?;
        let device = parse_device(&config.device)?;
        let module = {
            let modules = self.modules.read().unwrap();
            let module = modules
                .get(&module_id)
                .ok_or(Status::not_found("Not found"))?;
            Arc::clone(&module.data)
        };
        let model_client_info = {
            let modules_hashes = self.modules_hashes.read().unwrap();

            let model_hash = modules_hashes
                .get(&module_id)
                .ok_or(Status::not_found("Not found"))?;

            Arc::clone(&model_hash)
        };

        let dataset_client_info = {
            let datasets_hashes = self.datasets_hashes.read().unwrap();

            let dataset_hash = datasets_hashes
            .get(&dataset_id) 
            .ok_or(Status::not_found("Not found"))?;
            Arc::clone(&dataset_hash)
        };
        let dataset = {
            let datasets = self.datasets.read().unwrap();
            let dataset = datasets
                .get(&dataset_id)
                .ok_or(Status::not_found("Not found"))?;
            Arc::clone(&dataset.data)
        };
        Ok(stream_module_test(module, dataset, config, device, model_client_info, dataset_client_info).await)
    }

    async fn available_models(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<References>, Status> {
        let list = self
            .modules
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| Reference {
                identifier: format!("{}", k),
                description: v.description.clone(),
            })
            .collect();

        Ok(Response::new(References { list }))
    }

    async fn available_datasets(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<References>, Status> {
        let list = self
            .datasets
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| Reference {
                identifier: format!("{}", k),
                description: v.description.clone(),
            })
            .collect();

        Ok(Response::new(References { list }))
    }

    async fn available_devices(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Devices>, Status> {
        let mut list = vec![String::from("cpu")];
        if tch::Cuda::is_available() {
            list.push(String::from("gpu"));
            for index in 0..tch::Cuda::device_count() {
                list.push(format!("cuda:{}", index));
            }
        }

        Ok(Response::new(Devices { list }))
    }

    async fn available_optimizers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Optimizers>, Status> {
        let list = vec!["SGD", "Adam"].iter().map(|v| v.to_string()).collect();
        Ok(Response::new(Optimizers { list }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let logo_str: &str = include_str!("../logo.txt");
    let version_str: String = format!("VERSION : {}", env!("CARGO_PKG_VERSION"));
    let text_size: usize = 58;
    println!("{}\n", logo_str);
    fill_blank_and_print("BastionAI - SECURE AI TRAINING SERVER", text_size);
    fill_blank_and_print("MADE BY MITHRIL SECURITY", text_size);
    fill_blank_and_print(
        "GITHUB: https://github.com/mithril-security/bastionai",
        text_size,
    );
    fill_blank_and_print(&version_str, text_size);

    // Identity for untrusted (non-attested) communication
    let server_cert = fs::read("tls/host_server.pem")?;
    let server_key = fs::read("tls/host_server.key")?;
    let server_identity = Identity::from_pem(&server_cert, &server_key);

    let server = BastionAIServer::new();

    let mut file = File::open("config.toml")?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let network_config: bastionai_common::NetworkConfig = toml::from_str(&contents)?;

    let platform: CString = CString::new(format!("{}", whoami::platform())).unwrap();
    let uid: CString = {
        let mut hasher = DefaultHasher::new();
        whoami::username().hash(&mut hasher);
        whoami::hostname().hash(&mut hasher);
        platform.hash(&mut hasher);
        CString::new(format!("{:X}", hasher.finish())).unwrap()
    };

    if std::env::var("BASTIONAI_DISABLE_TELEMETRY").is_err() {
        telemetry::setup(platform.into_string().unwrap(), uid.into_string().unwrap())?;
    }
    else {
        info!(
            target: "BastionAI",
            "Telemetry is disabled.")
    }
    telemetry::add_event(TelemetryEventProps::Started {}, None);
    info!(
        target: "BastionAI",
        "BastionAI listening on {}",
        network_config.client_to_enclave_untrusted_socket()?
    );
    Server::builder()
        .tls_config(ServerTlsConfig::new().identity(server_identity))?
        .add_service(RemoteTorchServer::new(server))
        .serve(network_config.client_to_enclave_untrusted_socket()?)
        .await?;

    Ok(())
}
