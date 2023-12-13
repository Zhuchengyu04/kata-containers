// Copyright (c) 2023 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

// Allow Docker image config field names.
#![allow(non_snake_case)]
use crate::policy;
use crate::verity;

use anyhow::{anyhow, Result};
use containerd_client::services::v1::GetImageRequest;
use containerd_client::with_namespace;
use log::warn;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use sha2::{digest::typenum::Unsigned, digest::OutputSizeUser, Sha256};
use std::{io::Seek, io::Write, path::Path};
use tokio::fs;
use k8s_cri::v1::image_service_client::ImageServiceClient;
use std::collections::HashMap;
use std::convert::TryFrom;
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;
use tonic::Request;
use tokio::io;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// Container image properties obtained from an OCI repository.
#[derive(Clone, Debug, Default)]
pub struct Container {
    config_layer: DockerConfigLayer,
    image_layers: Vec<ImageLayer>,
}

/// Image config layer properties.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DockerConfigLayer {
    architecture: String,
    config: DockerImageConfig,
    rootfs: DockerRootfs,
}

/// Image config properties.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DockerImageConfig {
    User: Option<String>,
    Tty: Option<bool>,
    Env: Vec<String>,
    Cmd: Option<Vec<String>>,
    WorkingDir: Option<String>,
    Entrypoint: Option<Vec<String>>,
}

/// Container rootfs information.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DockerRootfs {
    r#type: String,
    diff_ids: Vec<String>,
}

/// This application's image layer properties.
#[derive(Clone, Debug)]
pub struct ImageLayer {
    pub diff_id: String,
    pub verity_hash: String,
}

const CONTAINERD_SOCKET_PATH : &str = "npipe:////./pipe/containerd-containerd";

impl Container {
    pub async fn new(use_cached_files: bool, image: &str) -> Result<Self> {
        info!("============================================");
        info!("Pulling image and layers for {:?}", image);

        let client = containerd_client::Client::from_path(CONTAINERD_SOCKET_PATH).await?;
        pull_image(image).await?;
        let manifest = get_image_manifest(image, &client).await?;
        let config_layer = get_config_layer(image).await.unwrap();          
        let image_layers = get_image_layers(
            use_cached_files,
            &manifest,
            &config_layer,
            &client
        ).await?;

        Ok(Container {
            config_layer,
            image_layers
        })
    }

    // Convert Docker image config to policy data.
    pub fn get_process(
        &self,
        process: &mut policy::KataProcess,
        yaml_has_command: bool,
        yaml_has_args: bool,
    ) {
        debug!("Getting process field from docker config layer...");
        let docker_config = &self.config_layer.config;

        if let Some(image_user) = &docker_config.User {
            if !image_user.is_empty() {
                debug!("Splitting Docker config user = {:?}", image_user);
                let user: Vec<&str> = image_user.split(':').collect();
                if !user.is_empty() {
                    debug!("Parsing uid from user[0] = {}", &user[0]);
                    match user[0].parse() {
                        Ok(id) => process.User.UID = id,
                        Err(e) => {
                            // "image: prom/prometheus" has user = "nobody", but
                            // process.User.UID is an u32 value.
                            warn!(
                                "Failed to parse {} as u32, using uid = 0 - error {e}",
                                &user[0]
                            );
                            process.User.UID = 0;
                        }
                    }
                }
                if user.len() > 1 {
                    debug!("Parsing gid from user[1] = {:?}", user[1]);
                    process.User.GID = user[1].parse().unwrap();
                }
            }
        }

        if let Some(terminal) = docker_config.Tty {
            process.Terminal = terminal;
        } else {
            process.Terminal = false;
        }

        for env in &docker_config.Env {
            process.Env.push(env.clone());
        }

        let policy_args = &mut process.Args;
        debug!("Already existing policy args: {:?}", policy_args);

        if let Some(entry_points) = &docker_config.Entrypoint {
            debug!("Image Entrypoint: {:?}", entry_points);
            if !yaml_has_command {
                debug!("Inserting Entrypoint into policy args");

                let mut reversed_entry_points = entry_points.clone();
                reversed_entry_points.reverse();

                for entry_point in reversed_entry_points {
                    policy_args.insert(0, entry_point.clone());
                }
            } else {
                debug!("Ignoring image Entrypoint because YAML specified the container command");
            }
        } else {
            debug!("No image Entrypoint");
        }

        debug!("Updated policy args: {:?}", policy_args);

        if yaml_has_command {
            debug!("Ignoring image Cmd because YAML specified the container command");
        } else if yaml_has_args {
            debug!("Ignoring image Cmd because YAML specified the container args");
        } else if let Some(commands) = &docker_config.Cmd {
            debug!("Adding to policy args the image Cmd: {:?}", commands);

            for cmd in commands {
                policy_args.push(cmd.clone());
            }
        } else {
            debug!("Image Cmd field is not present");
        }

        debug!("Updated policy args: {:?}", policy_args);

        if let Some(working_dir) = &docker_config.WorkingDir {
            if !working_dir.is_empty() {
                process.Cwd = working_dir.clone();
            }
        }

        debug!("get_process succeeded.");
    }

    pub fn get_image_layers(&self) -> Vec<ImageLayer> {
        self.image_layers.clone()
    }
}

async fn get_config_layer(image_ref: &str) ->  Result<DockerConfigLayer>{
    
    let channel = Endpoint::try_from("http://[::]")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| UnixStream::connect(CONTAINERD_SOCKET_PATH)))
        .await?;

    let mut client = ImageServiceClient::new(channel);

    let req =   k8s_cri::v1::ImageStatusRequest {
        image: Some(k8s_cri::v1::ImageSpec {
            image: image_ref.to_string(),
            annotations: HashMap::new(),
        }),
        verbose: true
    };

    let resp = client.image_status(req).await?;
    let image_layers = resp.into_inner();

    let status_info: serde_json::Value = serde_json::from_str(image_layers.info.get("info").unwrap())?;
    let image_spec = status_info["imageSpec"].as_object().unwrap();
    let docker_config_layer: DockerConfigLayer = serde_json::from_value(serde_json::to_value(image_spec)?)?;

    Ok(docker_config_layer)
}

pub async fn pull_image(image_ref: &str) ->  Result<()>{
    let channel = Endpoint::try_from("http://[::]")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| UnixStream::connect(CONTAINERD_SOCKET_PATH)))
        .await?;

    let mut client = ImageServiceClient::new(channel);

    let req =   k8s_cri::v1::PullImageRequest {
        image: Some(k8s_cri::v1::ImageSpec {
            image: image_ref.to_string(),
            annotations: HashMap::new(),
        }),
        auth: None,
        sandbox_config: None,
    };

    client.pull_image(req).await?;

    Ok(())
}

async fn get_content (digest: &str, client: &containerd_client::Client) ->  Result<serde_json::Value, anyhow::Error>{

    let req = containerd_client::services::v1::ReadContentRequest {
        digest: digest.to_string(),
        offset: 0,
        size: 0,
    };
    let req = with_namespace!(req, "k8s.io");
    let mut c = client.content();
    let resp = c.read(req).await?;
    let mut stream = resp.into_inner();

    while let Some(chunk) = stream.message().await? {
        if chunk.offset < 0 {
            return Err(anyhow!("Negative offset in chunk"));
        }
        else {
            return Ok(serde_json::from_slice(&chunk.data)?);
        }
    }

    Err(anyhow!("Unable to find content for digest: {}", digest))
}

async fn get_image_manifest (image_ref: &str, client: &containerd_client::Client) ->  Result<serde_json::Value>{

    let mut imageChannel = client.images();

    let req = GetImageRequest{
        name: image_ref.to_string()
    };
    let req = with_namespace!(req, "k8s.io");
    let resp = imageChannel.get(req).await?;

    let image_digest = resp.into_inner().image.unwrap().target.unwrap().digest;

    let content = get_content(&image_digest, &client).await?;
    let is_image_manifest = content.get("layers") != None;

    if is_image_manifest { // https://github.com/opencontainers/image-spec/blob/main/manifest.md
        return Ok(content);
    } 
    // else content is an image index https://github.com/opencontainers/image-spec/blob/main/image-index.md
    
    let image_index = content;
    let manifests = image_index["manifests"].as_array().unwrap();

    let mut manifestAmd64 = &serde_json::Value::Null;

    for entry in manifests {
        let platform = entry["platform"].as_object().unwrap();
        let architecture = platform["architecture"].as_str().unwrap();
        let os = platform["os"].as_str().unwrap();
        if architecture == "amd64" && os == "linux" {
            manifestAmd64 = entry;
            break;
        }
    }

    let image_digest = manifestAmd64["digest"].as_str().unwrap();
    
    Ok(get_content(image_digest, &client).await?)
}

async fn get_image_layers(
    use_cached_files: bool,
    manifest: &serde_json::Value,
    config_layer: &DockerConfigLayer,
    client: &containerd_client::Client
) -> Result<Vec<ImageLayer>> {
    let mut layer_index = 0;
    let mut layersVec = Vec::new();

    let layers = manifest["layers"].as_array().unwrap();
    
    for layer in layers {
        if layer["mediaType"].as_str().unwrap()
        .eq("application/vnd.docker.image.rootfs.diff.tar.gzip") {
            if layer_index < config_layer.rootfs.diff_ids.len() {
                let imageLayer = ImageLayer {
                    diff_id: config_layer.rootfs.diff_ids[layer_index].clone(),
                    verity_hash: get_verity_hash(
                        use_cached_files,
                        layer["digest"].as_str().unwrap(),
                        &client
                    ).await?,
                };
                layersVec.push(imageLayer);
            } else {
                return Err(anyhow!("Too many Docker gzip layers"));
            }
            layer_index += 1;
        }
    }

    Ok(layersVec)
}

fn delete_files(decompressed_path: &Path, compressed_path: &Path, verity_path: &Path) {
    let _ = fs::remove_file(&decompressed_path);
    let _ = fs::remove_file(&compressed_path);
    let _ = fs::remove_file(&verity_path);
}

async fn get_verity_hash(
    use_cached_files: bool,
    layer_digest: &str,
    client: &containerd_client::Client
) -> Result<String> {
    let base_dir = std::path::Path::new("layers_cache");

    // Use file names supported by both Linux and Windows.
    let file_name = str::replace(&layer_digest, ":", "-");

    let mut decompressed_path = base_dir.join(file_name);
    decompressed_path.set_extension("tar");

    let mut compressed_path = decompressed_path.clone();
    compressed_path.set_extension("gz");

    let mut verity_path = decompressed_path.clone();
    verity_path.set_extension("verity");

    let mut verity_hash = "".to_string();
    let mut error_message = "".to_string();
    let mut error = false;

    if use_cached_files && verity_path.exists() {
        info!("Using cached file {:?}", &verity_path);
    } else if let Err(e) = create_verity_hash_file(
        use_cached_files,
        layer_digest,
        &base_dir,
        &decompressed_path,
        &compressed_path,
        &verity_path,
        &client
    )
    .await
    {
        error = true;
        error_message = format!("Failed to create verity hash for {layer_digest}, error {e}");
    }

    if !error {
        match std::fs::read_to_string(&verity_path) {
            Err(e) => {
                error = true;
                error_message = format!("Failed to read {:?}, error {e}", &verity_path);
            }
            Ok(v) => {
                verity_hash = v;
                info!("dm-verity root hash: {verity_hash}");
            }
        }
    }

    if !use_cached_files {
        let _ = std::fs::remove_dir_all(&base_dir);
    } else if error {
        delete_files(&decompressed_path, &compressed_path, &verity_path);
    }

    if error {
        panic!("{error_message}");
    } else {
        Ok(verity_hash)
    }
}

async fn create_verity_hash_file(
    use_cached_files: bool,
    layer_digest: &str,
    base_dir: &Path,
    decompressed_path: &Path,
    compressed_path: &Path,
    verity_path: &Path,
    client: &containerd_client::Client
) -> Result<()> {
    if use_cached_files && decompressed_path.exists() {
        info!("Using cached file {:?}", &decompressed_path);
    } else {
        std::fs::create_dir_all(&base_dir)?;

        create_decompressed_layer_file(
            use_cached_files,
            layer_digest,
            &decompressed_path,
            &compressed_path,
            &client
        )
        .await?;
    }

    do_create_verity_hash_file(decompressed_path, verity_path)
}

async fn create_decompressed_layer_file(
    use_cached_files: bool,
    layer_digest: &str,
    decompressed_path: &Path,
    compressed_path: &Path,
    client: &containerd_client::Client
) -> Result<()> {
    if use_cached_files && compressed_path.exists() {
        info!("Using cached file {:?}", &compressed_path);
    } else {
        info!("Pulling layer {layer_digest}");
        let mut file = tokio::fs::File::create(&compressed_path)
            .await
            .map_err(|e| anyhow!(e)).expect("Failed to create file");

        info!("Decompressing layer");
    
        let req = containerd_client::services::v1::ReadContentRequest {
            digest: layer_digest.to_string(),
            offset: 0,
            size: 0,
        };
        let req = with_namespace!(req, "k8s.io");
        let mut c = client.content();
        let resp = c.read(req).await?;
        let mut stream = resp.into_inner();
    
        while let Some(chunk) = stream.message().await? {
            if chunk.offset < 0 {
                print!("oop")
            }
            file.seek(io::SeekFrom::Start(chunk.offset as u64)).await?;
            file.write_all(&chunk.data).await?;
        }
        
        file.flush().await.map_err(|e| anyhow!(e)).expect("Failed to flush file");
    }
    let compressed_file = std::fs::File::open(&compressed_path).map_err(|e| anyhow!(e))?;
    let mut decompressed_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&decompressed_path)?;
    let mut gz_decoder = flate2::read::GzDecoder::new(compressed_file);
    std::io::copy(&mut gz_decoder, &mut decompressed_file).map_err(|e| anyhow!(e))?;

    info!("Adding tarfs index to layer");
    decompressed_file.seek(std::io::SeekFrom::Start(0))?;
    tarindex::append_index(&mut decompressed_file).map_err(|e| anyhow!(e))?;
    decompressed_file.flush().map_err(|e| anyhow!(e))?;

    Ok(())
}

fn do_create_verity_hash_file(path: &Path, verity_path: &Path) -> Result<()> {
    info!("Calculating dm-verity root hash");
    let mut file = std::fs::File::open(path)?;
    let size = file.seek(std::io::SeekFrom::End(0))?;
    if size < 4096 {
        return Err(anyhow!("Block device {:?} is too small: {size}", &path));
    }

    let salt = [0u8; <Sha256 as OutputSizeUser>::OutputSize::USIZE];
    let v = verity::Verity::<Sha256>::new(size, 4096, 4096, &salt, 0)?;
    let hash = verity::traverse_file(&mut file, 0, false, v, &mut verity::no_write)?;
    let result = format!("{:x}", hash);

    let mut verity_file = std::fs::File::create(verity_path).map_err(|e| anyhow!(e))?;
    verity_file
        .write_all(result.as_bytes())
        .map_err(|e| anyhow!(e))?;
    verity_file.flush().map_err(|e| anyhow!(e))?;

    Ok(())
}

pub async fn get_container(use_cache: bool, image: &str) -> Result<Container> {
    Container::new(use_cache, image).await
}