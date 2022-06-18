use std::io::{Read, Seek, SeekFrom};
use std::{collections::VecDeque, convert::TryInto};

use crate::buffer::Buffer;
use crate::error::{Error, Result};
use crate::{bitmap::Bitmap, types::NativeType};

use super::super::compression;
use super::super::endianess::is_native_little_endian;
use super::{Compression, IpcBuffer, Node, OutOfSpecKind};

fn read_swapped<T: NativeType, R: Read + Seek>(
    reader: &mut R,
    length: usize,
    buffer: &mut Vec<T>,
    is_little_endian: bool,
) -> Result<()> {
    // slow case where we must reverse bits
    let mut slice = vec![0u8; length * std::mem::size_of::<T>()];
    reader.read_exact(&mut slice)?;

    let chunks = slice.chunks_exact(std::mem::size_of::<T>());
    if !is_little_endian {
        // machine is little endian, file is big endian
        buffer
            .as_mut_slice()
            .iter_mut()
            .zip(chunks)
            .try_for_each(|(slot, chunk)| {
                let a: T::Bytes = match chunk.try_into() {
                    Ok(a) => a,
                    Err(_) => unreachable!(),
                };
                *slot = T::from_be_bytes(a);
                Result::Ok(())
            })?;
    } else {
        // machine is big endian, file is little endian
        return Err(Error::NotYetImplemented(
            "Reading little endian files from big endian machines".to_string(),
        ));
    }
    Ok(())
}

fn read_uncompressed_buffer<T: NativeType, R: Read + Seek>(
    reader: &mut R,
    buffer_length: usize,
    length: usize,
    is_little_endian: bool,
) -> Result<Vec<T>> {
    let required_number_of_bytes = length.saturating_mul(std::mem::size_of::<T>());
    if required_number_of_bytes > buffer_length {
        return Err(Error::from(OutOfSpecKind::InvalidBuffer {
            length,
            type_name: std::any::type_name::<T>(),
            required_number_of_bytes,
            buffer_length,
        }));
        // todo: move this to the error's Display
        /*
        return Err(Error::OutOfSpec(
            format!("The slots of the array times the physical size must \
            be smaller or equal to the length of the IPC buffer. \
            However, this array reports {} slots, which, for physical type \"{}\", corresponds to {} bytes, \
            which is larger than the buffer length {}",
                length,
                std::any::type_name::<T>(),
                bytes,
                buffer_length,
            ),
        ));
         */
    }

    // it is undefined behavior to call read_exact on un-initialized, https://doc.rust-lang.org/std/io/trait.Read.html#tymethod.read
    // see also https://github.com/MaikKlein/ash/issues/354#issue-781730580
    let mut buffer = vec![T::default(); length];

    if is_native_little_endian() == is_little_endian {
        // fast case where we can just copy the contents
        let slice = bytemuck::cast_slice_mut(&mut buffer);
        reader.read_exact(slice)?;
    } else {
        read_swapped(reader, length, &mut buffer, is_little_endian)?;
    }
    Ok(buffer)
}

fn read_compressed_buffer<T: NativeType, R: Read + Seek>(
    reader: &mut R,
    buffer_length: usize,
    length: usize,
    is_little_endian: bool,
    compression: Compression,
) -> Result<Vec<T>> {
    if is_little_endian != is_native_little_endian() {
        return Err(Error::NotYetImplemented(
            "Reading compressed and big endian IPC".to_string(),
        ));
    }

    // it is undefined behavior to call read_exact on un-initialized, https://doc.rust-lang.org/std/io/trait.Read.html#tymethod.read
    // see also https://github.com/MaikKlein/ash/issues/354#issue-781730580
    let mut buffer = vec![T::default(); length];

    // decompress first
    // todo: move this allocation to an external buffer for re-use
    let mut slice = vec![0u8; buffer_length];
    reader.read_exact(&mut slice)?;

    let out_slice = bytemuck::cast_slice_mut(&mut buffer);

    let compression = compression
        .codec()
        .map_err(|err| Error::from(OutOfSpecKind::InvalidFlatbufferCompression(err)))?;

    match compression {
        arrow_format::ipc::CompressionType::Lz4Frame => {
            compression::decompress_lz4(&slice[8..], out_slice)?;
        }
        arrow_format::ipc::CompressionType::Zstd => {
            compression::decompress_zstd(&slice[8..], out_slice)?;
        }
    }
    Ok(buffer)
}

pub fn read_buffer<T: NativeType, R: Read + Seek>(
    buf: &mut VecDeque<IpcBuffer>,
    length: usize, // in slots
    reader: &mut R,
    block_offset: u64,
    is_little_endian: bool,
    compression: Option<Compression>,
) -> Result<Buffer<T>> {
    let buf = buf
        .pop_front()
        .ok_or_else(|| Error::from(OutOfSpecKind::ExpectedBuffer))?;

    let offset: u64 = buf
        .offset()
        .try_into()
        .map_err(|_| Error::from(OutOfSpecKind::NegativeFooterLength))?;

    let buffer_length: usize = buf
        .length()
        .try_into()
        .map_err(|_| Error::from(OutOfSpecKind::NegativeFooterLength))?;

    reader.seek(SeekFrom::Start(block_offset + offset))?;

    if let Some(compression) = compression {
        Ok(
            read_compressed_buffer(reader, buffer_length, length, is_little_endian, compression)?
                .into(),
        )
    } else {
        Ok(read_uncompressed_buffer(reader, buffer_length, length, is_little_endian)?.into())
    }
}

fn read_uncompressed_bitmap<R: Read + Seek>(
    length: usize,
    bytes: usize,
    reader: &mut R,
) -> Result<Vec<u8>> {
    if length > bytes * 8 {
        return Err(Error::from(OutOfSpecKind::InvalidBitmap {
            length,
            number_of_bits: bytes * 8,
        }));
    }
    // it is undefined behavior to call read_exact on un-initialized, https://doc.rust-lang.org/std/io/trait.Read.html#tymethod.read
    // see also https://github.com/MaikKlein/ash/issues/354#issue-781730580
    let mut buffer = vec![0; bytes];
    reader.read_exact(buffer.as_mut_slice())?;

    Ok(buffer)
}

fn read_compressed_bitmap<R: Read + Seek>(
    length: usize,
    bytes: usize,
    compression: Compression,
    reader: &mut R,
) -> Result<Vec<u8>> {
    let mut buffer = vec![0; (length + 7) / 8];

    // read all first
    // todo: move this allocation to an external buffer for re-use
    let mut slice = vec![0u8; bytes];
    reader.read_exact(&mut slice)?;

    let compression = compression
        .codec()
        .map_err(|err| Error::from(OutOfSpecKind::InvalidFlatbufferCompression(err)))?;

    match compression {
        arrow_format::ipc::CompressionType::Lz4Frame => {
            compression::decompress_lz4(&slice[8..], &mut buffer)?;
        }
        arrow_format::ipc::CompressionType::Zstd => {
            compression::decompress_zstd(&slice[8..], &mut buffer)?;
        }
    }
    Ok(buffer)
}

pub fn read_bitmap<R: Read + Seek>(
    buf: &mut VecDeque<IpcBuffer>,
    length: usize,
    reader: &mut R,
    block_offset: u64,
    _: bool,
    compression: Option<Compression>,
) -> Result<Bitmap> {
    let buf = buf
        .pop_front()
        .ok_or_else(|| Error::from(OutOfSpecKind::ExpectedBuffer))?;

    let offset: u64 = buf
        .offset()
        .try_into()
        .map_err(|_| Error::from(OutOfSpecKind::NegativeFooterLength))?;

    let bytes: usize = buf
        .length()
        .try_into()
        .map_err(|_| Error::from(OutOfSpecKind::NegativeFooterLength))?;

    reader.seek(SeekFrom::Start(block_offset + offset))?;

    let buffer = if let Some(compression) = compression {
        read_compressed_bitmap(length, bytes, compression, reader)
    } else {
        read_uncompressed_bitmap(length, bytes, reader)
    }?;

    Bitmap::try_new(buffer, length)
}

pub fn read_validity<R: Read + Seek>(
    buffers: &mut VecDeque<IpcBuffer>,
    field_node: Node,
    reader: &mut R,
    block_offset: u64,
    is_little_endian: bool,
    compression: Option<Compression>,
) -> Result<Option<Bitmap>> {
    let length: usize = field_node
        .length()
        .try_into()
        .map_err(|_| Error::from(OutOfSpecKind::NegativeFooterLength))?;

    Ok(if field_node.null_count() > 0 {
        Some(read_bitmap(
            buffers,
            length,
            reader,
            block_offset,
            is_little_endian,
            compression,
        )?)
    } else {
        let _ = buffers
            .pop_front()
            .ok_or_else(|| Error::from(OutOfSpecKind::ExpectedBuffer))?;
        None
    })
}
