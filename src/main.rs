use backend::{K8sSearch, SecretBackend, SecretBackendError};
use futures::{FutureExt, TryStreamExt};
use grpc::csi::v1::{
    identity_server::{Identity, IdentityServer},
    node_server::{Node, NodeServer},
    GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse, GetPluginInfoRequest,
    GetPluginInfoResponse, NodeExpandVolumeRequest, NodeExpandVolumeResponse,
    NodeGetCapabilitiesRequest, NodeGetCapabilitiesResponse, NodeGetInfoRequest,
    NodeGetInfoResponse, NodeGetVolumeStatsRequest, NodeGetVolumeStatsResponse,
    NodePublishVolumeRequest, NodePublishVolumeResponse, NodeStageVolumeRequest,
    NodeStageVolumeResponse, NodeUnpublishVolumeRequest, NodeUnpublishVolumeResponse,
    NodeUnstageVolumeRequest, NodeUnstageVolumeResponse, ProbeRequest, ProbeResponse,
};
use serde::{de::IntoDeserializer, Deserialize};
use std::{
    collections::HashMap,
    error::Error,
    os::unix::prelude::FileTypeExt,
    path::{Path, PathBuf},
};
use structopt::StructOpt;
use tokio::{
    fs::{create_dir_all, File},
    io::AsyncWriteExt,
    net::UnixListener,
    signal::unix::{signal, SignalKind},
};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{transport::Server, Request, Response, Status};
use utils::TonicUnixStream;

use crate::backend::SecretVolumeSelector;

mod backend;
mod grpc;
mod utils;

struct SecretProvisionerIdentity;

#[tonic::async_trait]
impl Identity for SecretProvisionerIdentity {
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        Ok(Response::new(GetPluginInfoResponse {
            name: "secrets.stackable.tech".to_string(),
            vendor_version: "0.0.0".to_string(),
            manifest: HashMap::new(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities: Vec::new(),
        }))
    }

    async fn probe(
        &self,
        _request: Request<ProbeRequest>,
    ) -> Result<Response<ProbeResponse>, Status> {
        Ok(Response::new(ProbeResponse { ready: Some(true) }))
    }
}

#[derive(thiserror::Error, Debug)]
enum PublishError {
    #[error("failed to parse selector from volume context")]
    InvalidSelector(#[source] serde::de::value::Error),
    #[error("backend failed to get secret data")]
    BackendGetSecretData(#[source] backend::k8s_search::Error),
    #[error("failed to create secret parent dir {1}")]
    CreateDir(#[source] std::io::Error, PathBuf),
    #[error("failed to create secret file {1}")]
    CreateFile(#[source] std::io::Error, PathBuf),
    #[error("failed to write secret file {1}")]
    WriteFile(#[source] std::io::Error, PathBuf),
}

impl From<PublishError> for tonic::Status {
    fn from(err: PublishError) -> Self {
        let mut full_msg = format!("{}", err);
        let mut curr_err = err.source();
        while let Some(curr_source) = curr_err {
            full_msg.push_str(&format!(": {}", err));
            curr_err = curr_source.source();
        }
        match err {
            PublishError::InvalidSelector(_) => tonic::Status::invalid_argument(full_msg),
            PublishError::BackendGetSecretData(err) => {
                tonic::Status::new(err.grpc_code(), full_msg)
            }
            PublishError::CreateDir(_, _) => tonic::Status::unavailable(full_msg),
            PublishError::CreateFile(_, _) => tonic::Status::unavailable(full_msg),
            PublishError::WriteFile(_, _) => tonic::Status::unavailable(full_msg),
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum UnpublishError {
    #[error("failed to clean up volume mount directory {1}")]
    Cleanup(#[source] std::io::Error, PathBuf),
}

impl From<UnpublishError> for tonic::Status {
    fn from(err: UnpublishError) -> Self {
        let mut full_msg = format!("{}", err);
        let mut curr_err = err.source();
        while let Some(curr_source) = curr_err {
            full_msg.push_str(&format!(": {}", err));
            curr_err = curr_source.source();
        }
        match err {
            UnpublishError::Cleanup(_, _) => tonic::Status::unavailable(full_msg),
        }
    }
}

struct SecretProvisionerNode {
    backend: K8sSearch,
}

impl SecretProvisionerNode {
    async fn save_secret_data(
        &self,
        target_path: &Path,
        data: HashMap<PathBuf, Vec<u8>>,
    ) -> Result<(), PublishError> {
        for (k, v) in data {
            let item_path = target_path.join(k);
            if let Some(item_path_parent) = item_path.parent() {
                create_dir_all(item_path_parent)
                    .await
                    .map_err(|err| PublishError::CreateDir(err, item_path_parent.into()))?;
            }
            File::create(item_path)
                .await
                .map_err(|err| PublishError::CreateFile(err, target_path.into()))?
                .write_all(&v)
                .await
                .map_err(|err| PublishError::WriteFile(err, target_path.into()))?;
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl Node for SecretProvisionerNode {
    async fn node_stage_volume(
        &self,
        _request: Request<NodeStageVolumeRequest>,
    ) -> Result<Response<NodeStageVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("endpoint not implemented"))
    }

    async fn node_unstage_volume(
        &self,
        _request: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("endpoint not implemented"))
    }

    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, tonic::Status> {
        let request = request.into_inner();
        let target_path = PathBuf::from(request.target_path);
        tracing::info!(
            volume.path = %target_path.display(),
            volume.ctx = ?request.volume_context,
            "Received NodePublishVolume request"
        );
        let sel = SecretVolumeSelector::deserialize(request.volume_context.into_deserializer())
            .map_err(PublishError::InvalidSelector)?;
        let data = self
            .backend
            .get_secret_data(sel)
            .await
            .map_err(PublishError::BackendGetSecretData)?;
        self.save_secret_data(&target_path, data).await?;
        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, tonic::Status> {
        let request = request.into_inner();
        let target_path = PathBuf::from(request.target_path);
        tracing::info!(
            volume.path = %target_path.display(),
            "Received NodeUnpublishVolume request"
        );
        tokio::fs::remove_dir_all(&target_path)
            .await
            .map_err(|err| UnpublishError::Cleanup(err, target_path))?;
        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_volume_stats(
        &self,
        _request: Request<NodeGetVolumeStatsRequest>,
    ) -> Result<Response<NodeGetVolumeStatsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("endpoint not implemented"))
    }

    async fn node_expand_volume(
        &self,
        _request: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("endpoint not implemented"))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, tonic::Status> {
        Ok(Response::new(NodeGetCapabilitiesResponse {
            capabilities: vec![],
        }))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, tonic::Status> {
        Ok(Response::new(NodeGetInfoResponse {
            node_id: "asdf".to_string(),
            max_volumes_per_node: i64::MAX,
            accessible_topology: None,
        }))
    }
}

#[derive(StructOpt)]
struct Opts {
    #[structopt(long, env)]
    csi_endpoint: PathBuf,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    stackable_operator::logging::initialize_logging("SECRET_PROVISIONER_LOG");
    let opts = Opts::from_args();
    let client =
        stackable_operator::client::create_client(Some("secrets.stackable.tech".to_string()))
            .await?;
    if opts
        .csi_endpoint
        .symlink_metadata()
        .map_or(false, |meta| meta.file_type().is_socket())
    {
        let _ = std::fs::remove_file(&opts.csi_endpoint);
    }
    let mut sigterm = signal(SignalKind::terminate())?;
    Server::builder()
        .add_service(
            tonic_reflection::server::Builder::configure()
                .include_reflection_service(true)
                .register_encoded_file_descriptor_set(grpc::FILE_DESCRIPTOR_SET_BYTES)
                .build()?,
        )
        .add_service(IdentityServer::new(SecretProvisionerIdentity))
        .add_service(NodeServer::new(SecretProvisionerNode {
            backend: backend::K8sSearch { client },
        }))
        .serve_with_incoming_shutdown(
            UnixListenerStream::new(UnixListener::bind(opts.csi_endpoint)?).map_ok(TonicUnixStream),
            sigterm.recv().map(|_| ()),
        )
        .await?;
    Ok(())
}
