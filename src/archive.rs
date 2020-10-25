use std::io::{Read, Seek};
use std::sync::{Arc, RwLock};

use super::error::*;
use super::seeker::*;
use super::table::*;
use super::util::*;

#[derive(Debug)]
/// Implementation of a MoPaQ archive viewer.
///
/// Refer to top-level documentation to see which features are supported.
///
/// Will work on any reader that implements `Read + Seek`.
pub struct Archive<R: Read + Seek> {
    seeker: Arc<RwLock<Seeker<R>>>,
    hash_table: FileHashTable,
    block_table: FileBlockTable,
}

impl<R: Read + Seek> Archive<R> {
    /// Try to open an MPQ archive from the specified `reader`.
    ///
    /// Immediately, this will perform the following:
    ///
    /// 1. Locate an MPQ header.
    /// 2. Locate and read the Hash Table.
    /// 3. Locate and read the Block Table.
    ///
    /// If any of these steps fail, the archive is deemed corrupted and
    /// an appropriate error is returned.
    ///
    /// No other operations will be performed.
    pub fn open(reader: R) -> Result<Archive<R>, Error> {
        let seeker = Arc::new(RwLock::new(Seeker::new(reader)?));

        let hash_table = FileHashTable::from_seeker(seeker.clone())?;
        let block_table = FileBlockTable::from_seeker(seeker.clone())?;

        Ok(Archive {
            seeker,
            hash_table,
            block_table,
        })
    }

    /// Read a file's contents.
    ///
    /// Notably, the filename resolution algorithm
    /// is case, and will treat backslashes (`\`) and forward slashes (`/`)
    /// as different characters.
    ///
    /// Does not support single-unit files or uncompressed files.
    pub fn read_file(&self, name: &str) -> Result<Vec<u8>, Error> {
        // find the hash entry and use it to find the block entry
        let hash_entry = self
            .hash_table
            .find_entry(name)
            .ok_or(Error::FileNotFound)?;
        let block_entry = self
            .block_table
            .get(hash_entry.block_index as usize)
            .ok_or(Error::FileNotFound)?;

        // calculate the file key
        let encryption_key = if block_entry.is_encrypted() {
            Some(calculate_file_key(
                name,
                block_entry.file_pos as u32,
                block_entry.uncompressed_size as u32,
                block_entry.is_key_adjusted(),
            ))
        } else {
            None
        };

        // read the sector offsets
        let sector_offsets = SectorOffsets::from_reader(
            self.seeker.clone(),
            block_entry,
            encryption_key.map(|k| k - 1),
        )?;

        // read out all the sectors
        let sector_range = sector_offsets.all();
        let raw_data = self
            .seeker
            .write()
            .map_err(|_| Error::PoisonedRWLock)?
            .read(
                block_entry.file_pos + u64::from(sector_range.0),
                u64::from(sector_range.1),
            )?;

        let mut result = Vec::with_capacity(block_entry.uncompressed_size as usize);

        let sector_size = self
            .seeker
            .read()
            .map_err(|_| Error::PoisonedRWLock)?
            .info()
            .sector_size;
        let sector_count = sector_offsets.count();
        let first_sector_offset = sector_offsets.one(0).unwrap().0;
        for i in 0..sector_count {
            let sector_offset = sector_offsets.one(i).unwrap();
            let slice_start = (sector_offset.0 - first_sector_offset) as usize;
            let slice_end = slice_start + sector_offset.1 as usize;

            // if this is the last sector, then its size will be less than
            // one archive sector size, so account for that
            let uncompressed_size = if (i + 1) == sector_count {
                let size = block_entry.uncompressed_size % sector_size;

                if size == 0 {
                    sector_size
                } else {
                    size
                }
            } else {
                sector_size
            };

            // decode the block and append it to the final result buffer
            let decoded_sector = decode_mpq_block(
                &raw_data[slice_start..slice_end],
                uncompressed_size,
                encryption_key.map(|k| k + i as u32),
            )?;

            result.extend(decoded_sector.iter());
        }

        Ok(result)
    }

    /// If the archive contains a `(listfile)`, this will method
    /// parse it and return a `Vec` containing all known filenames.
    pub fn files(&self) -> Option<Vec<String>> {
        let listfile = self.read_file("(listfile)").ok()?;

        let mut list = Vec::new();
        let mut line_start = 0;
        for i in 0..listfile.len() {
            let byte = listfile[i];

            if byte == b'\r' || byte == b'\n' {
                if i - line_start > 0 {
                    let line = &listfile[line_start..i];
                    let line = std::str::from_utf8(line);

                    if let Ok(line) = line {
                        list.push(line.to_string());
                    }
                }

                line_start = i + 1;
            }
        }

        Some(list)
    }

    // Returns the start of the archive in the reader, which is the MPQ header,
    // relative to the beginning of the reader.
    pub fn start(&self) -> u64 {
        self.seeker.read().unwrap().info().header_offset
    }

    // Returns the end of the archive in the reader, relative to the beginning of the reader.
    pub fn end(&self) -> u64 {
        self.seeker.read().unwrap().info().header_offset
            + self.seeker.read().unwrap().info().archive_size
    }

    // Returns the size of the archive as specified in the MPQ header.
    pub fn size(&self) -> u64 {
        self.seeker.read().unwrap().info().archive_size
    }
}
