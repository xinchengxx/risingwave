// Copyright 2022 Singularity Data
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

use std::collections::HashMap;

use bytes::{Buf, Bytes};
use itertools::Itertools;
use risingwave_common::array::RowDeserializer;
use risingwave_common::types::{display_datum_ref, to_datum_ref};
use risingwave_frontend::TableCatalog;
use risingwave_hummock_sdk::compaction_group::hummock_version_ext::HummockVersionExt;
use risingwave_hummock_sdk::key::{get_epoch, get_table_id, user_key};
use risingwave_hummock_sdk::HummockSstableId;
use risingwave_object_store::object::BlockLocation;
use risingwave_rpc_client::MetaClient;
use risingwave_storage::hummock::value::HummockValue;
use risingwave_storage::hummock::{
    Block, BlockHolder, BlockIterator, CompressionAlgorithm, SstableMeta, SstableStore,
};
use risingwave_storage::monitor::StoreLocalStatistic;

use crate::common::HummockServiceOpts;

type TableData = HashMap<u32, TableCatalog>;

pub async fn sst_dump() -> anyhow::Result<()> {
    // Retrieves the Sstable store so we can access the SstableMeta
    let mut hummock_opts = HummockServiceOpts::from_env()?;
    let (meta_client, hummock) = hummock_opts.create_hummock_store().await?;
    let version = hummock.inner().get_pinned_version().version();

    let table_data = load_table_schemas(&meta_client).await?;
    let sstable_store = &*hummock.sstable_store();
    for level in version.get_combined_levels() {
        for sstable_info in &level.table_infos {
            let id = sstable_info.id;

            let sstable_cache = sstable_store
                .sstable(sstable_info, &mut StoreLocalStatistic::default())
                .await?;
            let sstable = sstable_cache.value().as_ref();
            let sstable_meta = &sstable.meta;

            println!("SST id: {}", id);
            println!("-------------------------------------");
            println!("Level: {}", level.level_type);
            println!("File Size: {}", sstable_info.file_size);

            if let Some(key_range) = sstable_info.key_range.as_ref() {
                println!("Key Range:");
                println!(
                    "\tleft:\t{:?}\n\tright:\t{:?}\n\tinf:\t{:?}",
                    key_range.left, key_range.right, key_range.inf
                );
            } else {
                println!("Key Range: None");
            }

            println!("Estimated Table Size: {}", sstable_meta.estimated_size);
            println!("Bloom Filter Size: {}", sstable_meta.bloom_filter.len());
            println!("Key Count: {}", sstable_meta.key_count);
            println!("Version: {}", sstable_meta.version);

            print_blocks(id, &table_data, sstable_store, sstable_meta).await?;
        }
    }
    hummock_opts.shutdown().await;
    Ok(())
}

/// Determine all database tables and adds their information into a hash table with the table-ID as
/// key.
async fn load_table_schemas(meta_client: &MetaClient) -> anyhow::Result<TableData> {
    let mut tables = HashMap::new();

    let mvs = meta_client.risectl_list_state_tables().await?;
    mvs.iter().for_each(|tbl| {
        tables.insert(tbl.id, tbl.into());
    });

    Ok(tables)
}

/// Prints all blocks of a given SST including all contained KV-pairs.
async fn print_blocks(
    id: HummockSstableId,
    table_data: &TableData,
    sstable_store: &SstableStore,
    sstable_meta: &SstableMeta,
) -> anyhow::Result<()> {
    let data_path = sstable_store.get_sst_data_path(id);

    println!("Blocks:");
    for (i, block_meta) in sstable_meta.block_metas.iter().enumerate() {
        println!("\tBlock {}", i);
        println!("\t-----------");

        // Retrieve encoded block data in bytes
        let store = sstable_store.store();
        let block_loc = BlockLocation {
            offset: block_meta.offset as usize,
            size: block_meta.len as usize,
        };
        let block_data = store.read(&data_path, Some(block_loc)).await?;

        // Retrieve checksum and compression algorithm used from the encoded block data
        let len = block_data.len();
        let checksum = (&block_data[len - 8..]).get_u64_le();
        let compression = CompressionAlgorithm::decode(&mut &block_data[len - 9..len - 8])?;

        println!(
            "\tOffset: {}, Size: {}, Checksum: {}, Compression Algorithm: {:?}",
            block_meta.offset, block_meta.len, checksum, compression
        );

        print_kv_pairs(
            block_data,
            table_data,
            block_meta.uncompressed_size as usize,
        )?;
    }

    Ok(())
}

/// Prints the data of KV-Pairs of a given block out to the terminal.
fn print_kv_pairs(
    block_data: Bytes,
    table_data: &TableData,
    uncompressed_capacity: usize,
) -> anyhow::Result<()> {
    println!("\tKV-Pairs:");

    let block = Box::new(Block::decode(block_data, uncompressed_capacity).unwrap());
    let holder = BlockHolder::from_owned_block(block);
    let mut block_iter = BlockIterator::new(holder);
    block_iter.seek_to_first();

    while block_iter.is_valid() {
        let full_key = block_iter.key();
        let user_key = user_key(full_key);

        let full_val = block_iter.value();
        let humm_val = HummockValue::from_slice(block_iter.value())?;
        let (is_put, user_val) = match humm_val {
            HummockValue::Put(uval) => (true, uval),
            HummockValue::Delete => (false, &[] as &[u8]),
        };

        let epoch = get_epoch(full_key);

        println!("\t\t  full key: {:02x?}", full_key);
        println!("\t\tfull value: {:02x?}", full_val);
        println!("\t\t  user key: {:02x?}", user_key);
        println!("\t\tuser value: {:02x?}", user_val);
        println!("\t\t     epoch: {}", epoch);
        println!("\t\t      type: {}", if is_put { "Put" } else { "Delete" });

        print_table_column(full_key, user_val, table_data, is_put)?;

        println!();

        block_iter.next();
    }

    Ok(())
}

/// If possible, prints information about the table, column, and stored value.
fn print_table_column(
    full_key: &[u8],
    user_val: &[u8],
    table_data: &TableData,
    is_put: bool,
) -> anyhow::Result<()> {
    let table_id = match get_table_id(full_key) {
        None => return Ok(()),
        Some(table_id) => table_id,
    };

    print!("\t\t     table: {} - ", table_id);
    let table_catalog = match table_data.get(&table_id) {
        None => {
            // Table may have been dropped.
            println!("(unknown)");
            return Ok(());
        }
        Some(table) => table,
    };
    println!("{}", table_catalog.name);
    if !is_put {
        return Ok(());
    }

    let column_desc = table_catalog
        .value_indices
        .iter()
        .map(|idx| table_catalog.columns[*idx].column_desc.name.clone())
        .collect_vec();
    let data_types = table_catalog
        .value_indices
        .iter()
        .map(|idx| table_catalog.columns[*idx].data_type().clone())
        .collect_vec();
    let row_deserializer = RowDeserializer::new(data_types);
    let row = row_deserializer.deserialize(user_val)?;
    for (c, v) in column_desc.iter().zip_eq(row.0.iter()) {
        println!(
            "\t\t    column: {} {}",
            c,
            display_datum_ref(to_datum_ref(v))
        );
    }

    Ok(())
}
