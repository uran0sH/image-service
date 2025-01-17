// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use nydus_rafs::metadata::layout::toc;
use nydus_utils::digest::{DigestHasher, RafsDigest};
use nydus_utils::{compress, digest};
use sha2::Digest;

use crate::core::bootstrap::Bootstrap;
use crate::core::context::{
    ArtifactWriter, BlobContext, BlobManager, BootstrapContext, BootstrapManager, BuildContext,
    BuildOutput,
};
use crate::core::feature::Feature;
use crate::core::tree::Tree;

pub(crate) use self::directory::DirectoryBuilder;
pub(crate) use self::stargz::StargzBuilder;
pub(crate) use self::tarball::TarballBuilder;

mod directory;
mod stargz;
mod tarball;

/// Trait to generate a RAFS filesystem from the source.
pub(crate) trait Builder {
    fn build(
        &mut self,
        build_ctx: &mut BuildContext,
        bootstrap_mgr: &mut BootstrapManager,
        blob_mgr: &mut BlobManager,
    ) -> Result<BuildOutput>;
}

fn build_bootstrap(
    ctx: &mut BuildContext,
    bootstrap_mgr: &mut BootstrapManager,
    bootstrap_ctx: &mut BootstrapContext,
    blob_mgr: &mut BlobManager,
    mut tree: Tree,
) -> Result<Bootstrap> {
    let mut bootstrap = Bootstrap::new()?;
    // Merge with lower layer if there's one.
    if bootstrap_ctx.layered {
        let origin_bootstarp_offset = bootstrap_ctx.offset;
        // Disable prefetch and bootstrap.apply() will reset the prefetch enable/disable flag.
        ctx.prefetch.disable();
        bootstrap.build(ctx, bootstrap_ctx, &mut tree)?;
        tree = bootstrap.apply(ctx, bootstrap_ctx, bootstrap_mgr, blob_mgr, None)?;
        bootstrap_ctx.offset = origin_bootstarp_offset;
        bootstrap_ctx.layered = false;
    }

    // Convert the hierarchy tree into an array, stored in `bootstrap_ctx.nodes`.
    timing_tracer!(
        { bootstrap.build(ctx, bootstrap_ctx, &mut tree) },
        "build_bootstrap"
    )?;

    Ok(bootstrap)
}

fn dump_bootstrap(
    ctx: &mut BuildContext,
    bootstrap_mgr: &mut BootstrapManager,
    bootstrap_ctx: &mut BootstrapContext,
    bootstrap: &mut Bootstrap,
    blob_mgr: &mut BlobManager,
    blob_writer: &mut ArtifactWriter,
) -> Result<()> {
    if let Some((_, blob_ctx)) = blob_mgr.get_current_blob() {
        if blob_ctx.blob_id.is_empty() {
            // Make sure blob id is updated according to blob hash if user not specified.
            blob_ctx.blob_id = format!("{:x}", blob_ctx.blob_hash.clone().finalize());
        }
    }

    // Dump bootstrap file
    let blob_table = blob_mgr.to_blob_table(ctx)?;
    bootstrap.dump(
        ctx,
        &mut bootstrap_mgr.bootstrap_storage,
        bootstrap_ctx,
        &blob_table,
    )?;

    if ctx.blob_inline_meta {
        // Ensure the blob object is created in case of no chunks generated for the blob.
        let (_, blob_ctx) = blob_mgr
            .get_or_create_current_blob(ctx)
            .map_err(|_e| anyhow!("failed to get current blob object"))?;
        let bootstrap_offset = blob_writer.pos()?;
        let uncompressed_bootstrap = bootstrap_ctx.writer.as_bytes()?;
        let uncompressed_size = uncompressed_bootstrap.len();
        let uncompressed_digest =
            RafsDigest::from_buf(&uncompressed_bootstrap, digest::Algorithm::Sha256);

        // Output uncompressed data for backward compatibility and compressed data for new format.
        let (bootstrap_data, compressor) = if ctx.features.is_enabled(Feature::BlobToc) {
            let mut compressor = compress::Algorithm::Zstd;
            let (compressed_data, compressed) =
                compress::compress(&uncompressed_bootstrap, compressor)
                    .with_context(|| "failed to compress bootstrap".to_string())?;
            blob_ctx.write_data(blob_writer, &compressed_data)?;
            if !compressed {
                compressor = compress::Algorithm::None;
            }
            (compressed_data, compressor)
        } else {
            blob_ctx.write_data(blob_writer, &uncompressed_bootstrap)?;
            (uncompressed_bootstrap, compress::Algorithm::None)
        };

        let compressed_size = bootstrap_data.len();
        blob_ctx.write_tar_header(blob_writer, toc::ENTRY_BOOTSTRAP, compressed_size as u64)?;

        if ctx.features.is_enabled(Feature::BlobToc) {
            blob_ctx.entry_list.add(
                toc::ENTRY_BOOTSTRAP,
                compressor,
                uncompressed_digest,
                bootstrap_offset,
                compressed_size as u64,
                uncompressed_size as u64,
            )?;
        }
    }

    Ok(())
}

fn dump_toc(
    ctx: &mut BuildContext,
    blob_ctx: &mut BlobContext,
    blob_writer: &mut ArtifactWriter,
) -> Result<()> {
    if ctx.features.is_enabled(Feature::BlobToc) {
        let mut hasher = RafsDigest::hasher(digest::Algorithm::Sha256);
        let data = blob_ctx.entry_list.as_bytes().to_vec();
        let toc_size = data.len() as u64;
        blob_ctx.write_data(blob_writer, &data)?;
        hasher.digest_update(&data);
        let header = blob_ctx.write_tar_header(blob_writer, toc::ENTRY_TOC, toc_size)?;
        hasher.digest_update(header.as_bytes());
        blob_ctx.rafs_blob_toc_digest = hasher.digest_finalize().data;
        blob_ctx.rafs_blob_toc_size = toc_size as u32 + header.as_bytes().len() as u32;
    }
    Ok(())
}

fn finalize_blob(
    ctx: &mut BuildContext,
    blob_mgr: &mut BlobManager,
    blob_writer: &mut ArtifactWriter,
) -> Result<()> {
    if let Some((_, blob_ctx)) = blob_mgr.get_current_blob() {
        dump_toc(ctx, blob_ctx, blob_writer)?;
        let hash = blob_ctx.blob_hash.clone().finalize();
        if ctx.blob_id.is_empty() {
            blob_ctx.blob_id = format!("{:x}", hash);
        }
        blob_ctx.rafs_blob_digest = hash.into();
        blob_ctx.rafs_blob_size = blob_ctx.compressed_blob_size;
        blob_writer.finalize(blob_ctx.blob_id())?;
    }
    Ok(())
}
