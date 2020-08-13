use anyhow::{bail, format_err, Error};
use std::collections::{HashSet, HashMap};
use std::sync::{Mutex, Arc};
use std::os::raw::c_int;

use futures::future::{Future, TryFutureExt};
use serde_json::json;

use proxmox_backup::backup::*;
use proxmox_backup::client::*;

use crate::registry::Registry;
use crate::capi_types::*;
use crate::upload_queue::*;

use lazy_static::lazy_static;

lazy_static!{
    static ref PREVIOUS_CSUMS: Mutex<HashMap<String, [u8;32]>> = {
        Mutex::new(HashMap::new())
    };
}

pub struct ImageUploadInfo {
    wid: u64,
    device_name: String,
    zero_chunk_digest: [u8; 32],
    device_size: u64,
    upload_queue: Option<UploadQueueSender>,
    upload_result: Option<UploadResultReceiver>,
}


// Note: We alway register/upload a chunk containing zeros
async fn register_zero_chunk(
    client: Arc<BackupWriter>,
    crypt_config: Option<Arc<CryptConfig>>,
    chunk_size: usize,
    wid: u64,
) -> Result<[u8;32], Error> {

    let (chunk, zero_chunk_digest) = DataChunkBuilder::build_zero_chunk(
        crypt_config.as_ref().map(Arc::as_ref),
        chunk_size,
        true,
    )?;
    let chunk_data = chunk.into_inner();

    let param = json!({
        "wid": wid,
        "digest": proxmox::tools::digest_to_hex(&zero_chunk_digest),
        "size": chunk_size,
        "encoded-size": chunk_data.len(),
    });

    client.upload_post("fixed_chunk", Some(param), "application/octet-stream", chunk_data).await?;

    Ok(zero_chunk_digest)
}

pub(crate) async fn add_config(
    client: Arc<BackupWriter>,
    manifest: Arc<Mutex<BackupManifest>>,
    name: String,
    data: Vec<u8>,
    compress: bool,
    crypt_mode: CryptMode,
) -> Result<c_int, Error> {
    //println!("add config {} size {}", name, size);

    let blob_name = format!("{}.blob", name);

    let stats = client.upload_blob_from_data(data, &blob_name, compress, crypt_mode == CryptMode::Encrypt).await?;

    let mut guard = manifest.lock().unwrap();
    guard.add_file(blob_name, stats.size, stats.csum, crypt_mode)?;

    Ok(0)
}

fn archive_name(device_name: &str) -> String {
    format!("{}.img.fidx", device_name)
}

pub(crate) fn check_last_incremental_csum(
    manifest: Arc<BackupManifest>,
    device_name: &str,
    device_size: u64,
) -> bool {

    match PREVIOUS_CSUMS.lock().unwrap().get(device_name) {
        Some(csum) => manifest.verify_file(&archive_name(device_name), &csum, device_size).is_ok(),
        None => false,
    }
}

pub(crate) fn check_last_encryption_mode(
    manifest: Arc<BackupManifest>,
    device_name: &str,
    crypt_mode: CryptMode,
) -> bool {
    match manifest.lookup_file_info(&archive_name(device_name)) {
        Ok(file) => {
            match (file.crypt_mode, crypt_mode) {
                (CryptMode::Encrypt, CryptMode::Encrypt) => true,
                (CryptMode::Encrypt, _) => false,
                (CryptMode::SignOnly, CryptMode::Encrypt) => false,
                (CryptMode::SignOnly, _) => true,
                (CryptMode::None, CryptMode::Encrypt) => false,
                (CryptMode::None, _) => true,
            }
        },
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn register_image(
    client: Arc<BackupWriter>,
    crypt_config: Option<Arc<CryptConfig>>,
    crypt_mode: CryptMode,
    manifest: Option<Arc<BackupManifest>>,
    registry: Arc<Mutex<Registry<ImageUploadInfo>>>,
    known_chunks: Arc<Mutex<HashSet<[u8;32]>>>,
    device_name: String,
    device_size: u64,
    chunk_size: u64,
    incremental: bool,
) -> Result<c_int, Error> {

    let archive_name = archive_name(&device_name);

    let index = match manifest {
        Some(manifest) => {
            match client.download_previous_fixed_index(&archive_name, &manifest, Arc::clone(&known_chunks)).await {
                Ok(index) => Some(index),
                // not having a previous index is not fatal, so ignore errors
                Err(_) => None
            }
        },
        None => None
    };

    let mut param = json!({ "archive-name": archive_name , "size": device_size });
    let mut initial_index = Arc::new(None);

    if incremental {
        let csum = {
            let map = PREVIOUS_CSUMS.lock().unwrap();
            match map.get(&device_name) {
                Some(c) => Some(*c),
                None => None
            }
        };

        if let Some(csum) = csum {
            param["reuse-csum"] = proxmox::tools::digest_to_hex(&csum).into();

            match index {
                Some(index) => {
                    let index_size = ((device_size + chunk_size -1)/chunk_size) as usize;
                    if index_size != index.index_count() {
                        bail!("previous backup has different size than current state, cannot do incremental backup (drive: {})", archive_name);
                    }
                    if index.compute_csum().0 != csum {
                        bail!("previous backup checksum doesn't match session cache, incremental backup would be out of sync (drive: {})", archive_name);
                    }

                    initial_index = Arc::new(Some(index));
                },
                None => bail!("no previous backup found, cannot do incremental backup")
            }

        } else {
            bail!("no previous backups in this session, cannot do incremental backup");
        }
    }

    let wid = client.post("fixed_index", Some(param)).await?.as_u64().unwrap();

    let zero_chunk_digest = register_zero_chunk(
        Arc::clone(&client),
        if crypt_mode == CryptMode::Encrypt { crypt_config } else { None },
        chunk_size as usize,
        wid,
    ).await?;

    let (upload_queue, upload_result) = create_upload_queue(
        Arc::clone(&client),
        Arc::clone(&known_chunks),
        Arc::clone(&initial_index),
        wid,
        device_size,
        chunk_size,
    );

    let info = ImageUploadInfo {
        wid,
        device_name,
        zero_chunk_digest,
        device_size,
        upload_queue: Some(upload_queue),
        upload_result: Some(upload_result),
   };

    let mut guard = registry.lock().unwrap();
    let dev_id = guard.register(info)?;

    Ok(dev_id as c_int)
}

pub(crate) async fn close_image(
    client: Arc<BackupWriter>,
    manifest: Arc<Mutex<BackupManifest>>,
    registry: Arc<Mutex<Registry<ImageUploadInfo>>>,
    dev_id: u8,
    crypt_mode: CryptMode,
) -> Result<c_int, Error> {

    //println!("close image {}", dev_id);

    let (wid, upload_result, device_name, device_size) = {
        let mut guard = registry.lock().unwrap();
        let info = guard.lookup(dev_id)?;

        info.upload_queue.take(); // close

        (info.wid, info.upload_result.take(), info.device_name.clone(), info.device_size)
    };

    let upload_result = match upload_result {
        Some(upload_result) => {
            match upload_result.await? {
                Ok(res) => res,
                Err(err) => bail!("close_image: upload error: {}", err),
            }
        }
        None => bail!("close_image: unknown error because upload result channel was already closed"),
    };

    let csum = proxmox::tools::digest_to_hex(&upload_result.csum);

    let param = json!({
        "wid": wid ,
        "chunk-count": upload_result.chunk_count,
        "size": upload_result.bytes_written,
        "csum": csum.clone(),
    });

    let _value = client.post("fixed_close", Some(param)).await?;

    let mut guard = registry.lock().unwrap();
    let info = guard.lookup(dev_id)?;
    let mut prev_csum_guard = PREVIOUS_CSUMS.lock().unwrap();
    prev_csum_guard.insert(info.device_name.clone(), upload_result.csum);


    let mut guard = manifest.lock().unwrap();
    guard.add_file(format!("{}.img.fidx", device_name), device_size, upload_result.csum, crypt_mode)?;

    Ok(0)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_data(
    client: Arc<BackupWriter>,
    crypt_config: Option<Arc<CryptConfig>>,
    registry: Arc<Mutex<Registry<ImageUploadInfo>>>,
    known_chunks: Arc<Mutex<HashSet<[u8;32]>>>,
    dev_id: u8,
    data: DataPointer,
    offset: u64,
    size: u64, // actual data size
    chunk_size: u64, // expected data size
    compress: bool,
) -> Result<c_int, Error> {

    //println!("dev {}: write {} {}", dev_id, offset, size);

    let (wid, mut upload_queue, zero_chunk_digest) = {
        let mut guard = registry.lock().unwrap();
        let info = guard.lookup(dev_id)?;


        (info.wid, info.upload_queue.clone(), info.zero_chunk_digest)
    };

    let mut reused = false;

    let upload_future: Box<dyn Future<Output = Result<ChunkUploadInfo, Error>> + Send + Unpin> = {
        if data.0.is_null() {
            if size != chunk_size {
                bail!("write_data: got invalid null chunk");
            }
            let upload_info = ChunkUploadInfo { digest: zero_chunk_digest, offset, size, chunk_is_known: true };
            reused = true;
            Box::new(futures::future::ok(upload_info))
        } else {
            let data: &[u8] = unsafe { std::slice::from_raw_parts(data.0, size as usize) };

            let mut chunk_builder = DataChunkBuilder::new(data).compress(compress);

            if let Some(ref crypt_config) = crypt_config {
                chunk_builder = chunk_builder.crypt_config(crypt_config);
            }

            let digest = chunk_builder.digest();

            let chunk_is_known = {
                let known_chunks_guard = known_chunks.lock().unwrap();
                known_chunks_guard.contains(digest)
            };

            if chunk_is_known {
                let upload_info = ChunkUploadInfo { digest: *digest, offset, size, chunk_is_known: true };
                reused = true;
                Box::new(futures::future::ok(upload_info))
           } else {
                let (chunk, digest) = chunk_builder.build()?;
                let digest_str = proxmox::tools::digest_to_hex(&digest);
                let chunk_data = chunk.into_inner();

                let param = json!({
                    "wid": wid,
                    "digest": digest_str,
                    "size": size,
                    "encoded-size": chunk_data.len(),
                });

                // Phase 1: send data
                let response_future = client.send_upload_request(
                    "POST",
                    "fixed_chunk",
                    Some(param),
                    "application/octet-stream",
                    chunk_data,
                ).await?;

                // create response future (run that in other task)
                let upload_future = response_future
                    .map_err(Error::from)
                    .and_then(H2Client::h2api_response)
                    .map_ok(move |_| {
                        ChunkUploadInfo { digest, offset, size, chunk_is_known: false }
                    })
                    .map_err(|err| format_err!("pipelined request failed: {}", err));

                Box::new(Box::pin(upload_future))
            }
        }
    };

    match upload_queue {
        Some(ref mut upload_queue) => {
            // Phase 2: send reponse future to other task
            if upload_queue.send(upload_future).await.is_err() {
                let upload_result = {
                    let mut guard = registry.lock().unwrap();
                    let info = guard.lookup(dev_id)?;
                    info.upload_queue.take(); // close
                    info.upload_result.take()
                };
                match upload_result {
                    Some(upload_result) => {
                        match upload_result.await? {
                            Ok(res) => res,
                            Err(err) => bail!("write_data upload error: {}", err),
                        }
                    }
                    None => bail!("write_data: unknown error because upload result channel was already closed"),
                };
            }
        }
        None => {
            bail!("upload queue already closed");
        }
    }

    //println!("upload chunk sucessful");

    Ok(if reused { 0 } else { size as c_int })
}

pub(crate) async fn finish_backup(
    client: Arc<BackupWriter>,
    crypt_config: Option<Arc<CryptConfig>>,
    manifest: Arc<Mutex<BackupManifest>>,
) -> Result<c_int, Error> {

    let manifest = {
        let guard = manifest.lock().unwrap();
        guard.to_string(crypt_config.as_ref().map(Arc::as_ref))
            .map_err(|err| format_err!("unable to format manifest - {}", err))?
    };

    client
        .upload_blob_from_data(manifest.into_bytes(), MANIFEST_BLOB_NAME, true, false)
        .await?;

    client.finish().await?;

    Ok(0)
}
