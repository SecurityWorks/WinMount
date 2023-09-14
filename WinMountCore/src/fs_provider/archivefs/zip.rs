use std::{
    borrow::Cow,
    collections::BTreeMap,
    io::{BufReader, Read, Seek},
    mem::MaybeUninit,
    sync::RwLock,
};

use byteorder::{LittleEndian, ReadBytesExt};

use crate::{
    fs_provider::{
        CursorFile, FileAttributes, FileStatInfo, FileSystemError, FileSystemResult, OwnedFile,
        PathDelimiter, SegPath,
    },
    util::{calculate_hash, CaselessStr, CaselessString},
};

use super::{ArchiveNonUnicodeCompatConfig, ArchiveNonUnicodeEncoding};

const ZIP_COMPRESSION_STORE: u16 = 0;
const ZIP_COMPRESSION_DEFLATE: u16 = 8;

const ZIP_GPFLAGS_ENCRYPTED: u16 = 0x1;
// Language encoding flag
const ZIP_GPFLAGS_EFS: u16 = 0x800;

#[derive(Debug)]
struct ZipEndOfCentralDirRecord {
    num_disk: u16,
    num_disk_central_dir_start: u16,
    total_central_dir_records_this_disk: u16,
    total_central_dir_records: u16,
    size_central_dir: u32,
    offset_central_dir: u32,
    comment: Vec<u8>,
}

#[derive(Debug)]
struct ZipCentralDirRecord {
    made_by_ver: u16,
    min_extract_ver: u16,
    general_purpose_bitflag: u16,
    compression_method: u16,
    last_modify_time: u16,
    last_modify_date: u16,
    uncompressed_data_crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    file_name: Vec<u8>,
    extra_data: Vec<u8>,
    comment: Vec<u8>,
    num_disk_start: u16,
    internal_file_attributes: u16,
    external_file_attributes: u32,
    local_file_header_offset: u32,
}

#[derive(Debug)]
struct ZipLocalFileRecord {
    min_extract_ver: u16,
    general_purpose_bitflag: u16,
    compression_method: u16,
    last_modify_time: u16,
    last_modify_date: u16,
    uncompressed_data_crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    file_name: Vec<u8>,
    extra_data: Vec<u8>,
}

// NOTE: All these sizes only account for static fields (e.g. file names are not included)
const ZIP_LOCAL_FILE_HEADER_SIZE: u32 = 30;

struct ZipFolderEntry {
    children: BTreeMap<CaselessString, ZipEntry>,
    index: u64,
}

struct ZipFileEntry {
    data: ZipCentralDirRecord,
    index: u64,
}

enum ZipEntry {
    Folder(ZipFolderEntry),
    File(ZipFileEntry),
}

const ZERO_TIME: std::time::SystemTime = std::time::SystemTime::UNIX_EPOCH;

impl ZipFolderEntry {
    fn get_file_stat_info(&self) -> FileStatInfo {
        FileStatInfo {
            index: self.index,
            size: 0,
            is_dir: true,
            attributes: FileAttributes::DirectoryFile,
            creation_time: ZERO_TIME,
            last_access_time: ZERO_TIME,
            last_write_time: ZERO_TIME,
        }
    }
}

impl ZipFileEntry {
    fn get_file_stat_info(&self) -> FileStatInfo {
        FileStatInfo {
            index: self.index,
            size: self.data.uncompressed_size as _,
            is_dir: false,
            attributes: FileAttributes::empty(),
            creation_time: ZERO_TIME,
            last_access_time: ZERO_TIME,
            last_write_time: ZERO_TIME,
        }
    }
}

impl ZipEntry {
    fn is_dir(&self) -> bool {
        matches!(self, Self::Folder(_))
    }
    fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }
    fn as_borrowed(&self) -> BorrowedZipEntry {
        match self {
            Self::Folder(e) => BorrowedZipEntry::Folder(e),
            Self::File(e) => BorrowedZipEntry::File(e),
        }
    }
    fn get_file_stat_info(&self) -> FileStatInfo {
        self.as_borrowed().get_file_stat_info()
    }
}

#[derive(Clone, Copy)]
enum BorrowedZipEntry<'a> {
    Folder(&'a ZipFolderEntry),
    File(&'a ZipFileEntry),
}

impl BorrowedZipEntry<'_> {
    fn is_dir(&self) -> bool {
        matches!(self, Self::Folder(_))
    }
    fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }
    fn get_file_stat_info(&self) -> FileStatInfo {
        match self {
            Self::Folder(e) => e.get_file_stat_info(),
            Self::File(e) => e.get_file_stat_info(),
        }
    }
}

pub struct ZipArchive<'a> {
    file: OwnedFile<'a>,
    file_index: u64,
    eocd: ZipEndOfCentralDirRecord,
    cd: ZipFolderEntry,
}

// TODO: Support Zip64

fn parse_eocd_record(
    file: &OwnedFile,
    file_len: u64,
) -> Result<ZipEndOfCentralDirRecord, FileSystemError> {
    const END_SIGNATURE: u32 = 0x06054b50;
    const END_MIN_SIZE: u64 = 22;
    const END_MAX_SIZE: u64 = END_MIN_SIZE + u16::MAX as u64;

    // Central directory record sits at the end of the zip file
    if file_len < END_MIN_SIZE {
        return Err(anyhow::anyhow!("invalid zip file").into());
    }

    fn parse_exact(mut buf: &[u8]) -> anyhow::Result<ZipEndOfCentralDirRecord> {
        if buf.len() < END_MIN_SIZE as _ {
            anyhow::bail!("EOCD record too small");
        }
        // let buf = std::io::Cursor::new(buf);
        if buf.read_u32::<LittleEndian>()? != END_SIGNATURE {
            anyhow::bail!("invalid signature for EOCD record");
        }
        let num_disk = buf.read_u16::<LittleEndian>()?;
        let num_disk_central_dir_start = buf.read_u16::<LittleEndian>()?;
        let total_central_dir_records_this_disk = buf.read_u16::<LittleEndian>()?;
        let total_central_dir_records = buf.read_u16::<LittleEndian>()?;
        let size_central_dir = buf.read_u32::<LittleEndian>()?;
        let offset_central_dir = buf.read_u32::<LittleEndian>()?;
        let size_comment = buf.read_u16::<LittleEndian>()?;
        if buf.len() != size_comment as usize {
            anyhow::bail!("EOCD record has bad comment data");
        }
        let comment = buf.to_owned();
        Ok(ZipEndOfCentralDirRecord {
            num_disk,
            num_disk_central_dir_start,
            total_central_dir_records_this_disk,
            total_central_dir_records,
            size_central_dir,
            offset_central_dir,
            comment,
        })
    }

    let offset_lower_bound = file_len.saturating_sub(END_MAX_SIZE);

    // Read minimum bytes first
    let mut buf = [0; END_MIN_SIZE as _];
    file.read_at_exact(file_len - END_MIN_SIZE, &mut buf)?;
    if let Ok(record) = parse_exact(&buf) {
        return Ok(record);
    }

    // Try again with full data
    let buf = {
        let len = (file_len - offset_lower_bound) as _;
        let mut full_vec = vec![0; len];
        file.read_at_exact(offset_lower_bound, &mut full_vec[..len])?;
        full_vec.extend_from_slice(&buf);
        full_vec
    };
    let mut buf_view = &buf[..];
    while buf_view.len() > END_MIN_SIZE as usize {
        if &buf_view[..4] == END_SIGNATURE.to_le_bytes() {
            if let Ok(record) = parse_exact(buf_view) {
                return Ok(record);
            }
        }
        buf_view = &buf_view[1..];
    }

    Err(anyhow::anyhow!("could not find EOCD record").into())
}

fn parse_central_dir_record_list(
    file: &OwnedFile,
    eocd: &ZipEndOfCentralDirRecord,
) -> Result<Vec<ZipCentralDirRecord>, FileSystemError> {
    const CD_HEADER_SIGNATURE: u32 = 0x02014b50;
    const CD_HEADER_SIZE: u32 = 46;

    let file = CursorFile::with_position(file, eocd.offset_central_dir as _);
    let mut file = BufReader::new(file);

    fn parse_one(file: &mut impl std::io::Read) -> anyhow::Result<ZipCentralDirRecord> {
        if file.read_u32::<LittleEndian>()? != CD_HEADER_SIGNATURE {
            anyhow::bail!("invalid signature for Central Directory record");
        }
        let made_by_ver = file.read_u16::<LittleEndian>()?;
        let min_extract_ver = file.read_u16::<LittleEndian>()?;
        let general_purpose_bitflag = file.read_u16::<LittleEndian>()?;
        let compression_method = file.read_u16::<LittleEndian>()?;
        let last_modify_time = file.read_u16::<LittleEndian>()?;
        let last_modify_date = file.read_u16::<LittleEndian>()?;
        let uncompressed_data_crc32 = file.read_u32::<LittleEndian>()?;
        let compressed_size = file.read_u32::<LittleEndian>()?;
        let uncompressed_size = file.read_u32::<LittleEndian>()?;
        let len_file_name = file.read_u16::<LittleEndian>()?;
        let len_extra_data = file.read_u16::<LittleEndian>()?;
        let len_comment = file.read_u16::<LittleEndian>()?;
        let num_disk_start = file.read_u16::<LittleEndian>()?;
        let internal_file_attributes = file.read_u16::<LittleEndian>()?;
        let external_file_attributes = file.read_u32::<LittleEndian>()?;
        let local_file_header_offset = file.read_u32::<LittleEndian>()?;
        let mut file_name = vec![0; len_file_name as _];
        file.read_exact(&mut file_name)?;
        let mut extra_data = vec![0; len_extra_data as _];
        file.read_exact(&mut extra_data)?;
        let mut comment = vec![0; len_comment as _];
        file.read_exact(&mut comment)?;

        Ok(ZipCentralDirRecord {
            made_by_ver,
            min_extract_ver,
            general_purpose_bitflag,
            compression_method,
            last_modify_time,
            last_modify_date,
            uncompressed_data_crc32,
            compressed_size,
            uncompressed_size,
            file_name,
            extra_data,
            comment,
            num_disk_start,
            internal_file_attributes,
            external_file_attributes,
            local_file_header_offset,
        })
    }

    let mut records = Vec::with_capacity(eocd.total_central_dir_records as _);
    for _ in 0..eocd.total_central_dir_records {
        records.push(parse_one(&mut file)?);
    }

    Ok(records)
}

fn parse_local_file_record(file: &mut impl std::io::Read) -> anyhow::Result<ZipLocalFileRecord> {
    const LOCAL_HEADER_SIGNATURE: u32 = 0x04034b50;

    if file.read_u32::<LittleEndian>()? != LOCAL_HEADER_SIGNATURE {
        anyhow::bail!("invalid signature for Local File record");
    }
    let min_extract_ver = file.read_u16::<LittleEndian>()?;
    let general_purpose_bitflag = file.read_u16::<LittleEndian>()?;
    let compression_method = file.read_u16::<LittleEndian>()?;
    let last_modify_time = file.read_u16::<LittleEndian>()?;
    let last_modify_date = file.read_u16::<LittleEndian>()?;
    let uncompressed_data_crc32 = file.read_u32::<LittleEndian>()?;
    let compressed_size = file.read_u32::<LittleEndian>()?;
    let uncompressed_size = file.read_u32::<LittleEndian>()?;
    let len_file_name = file.read_u16::<LittleEndian>()?;
    let len_extra_data = file.read_u16::<LittleEndian>()?;
    let mut file_name = vec![0; len_file_name as _];
    file.read_exact(&mut file_name)?;
    let mut extra_data = vec![0; len_extra_data as _];
    file.read_exact(&mut extra_data)?;

    Ok(ZipLocalFileRecord {
        min_extract_ver,
        general_purpose_bitflag,
        compression_method,
        last_modify_time,
        last_modify_date,
        uncompressed_data_crc32,
        compressed_size,
        uncompressed_size,
        file_name,
        extra_data,
    })
}

// WARN: This function does not verify the correctness of local headers!
fn skip_local_file_record_with_central_no_verify(
    file: &mut (impl std::io::Read + std::io::Seek),
    central: &ZipCentralDirRecord,
) -> anyhow::Result<()> {
    const LOCAL_HEADER_SIGNATURE: u32 = 0x04034b50;

    let mut buf = [0; ZIP_LOCAL_FILE_HEADER_SIZE as _];
    file.read_exact(&mut buf)?;
    let mut buf_view = buf.as_slice();

    if buf_view.read_u32::<LittleEndian>()? != LOCAL_HEADER_SIGNATURE {
        anyhow::bail!("invalid signature for Local File record");
    }
    let min_extract_ver = buf_view.read_u16::<LittleEndian>()?;
    let general_purpose_bitflag = buf_view.read_u16::<LittleEndian>()?;
    let compression_method = buf_view.read_u16::<LittleEndian>()?;
    let last_modify_time = buf_view.read_u16::<LittleEndian>()?;
    let last_modify_date = buf_view.read_u16::<LittleEndian>()?;
    let uncompressed_data_crc32 = buf_view.read_u32::<LittleEndian>()?;
    let compressed_size = buf_view.read_u32::<LittleEndian>()?;
    let uncompressed_size = buf_view.read_u32::<LittleEndian>()?;
    let len_file_name = buf_view.read_u16::<LittleEndian>()?;
    let len_extra_data = buf_view.read_u16::<LittleEndian>()?;
    // TODO: Some fields seems to be missing in central directory, despite local having these?
    file.seek(std::io::SeekFrom::Current(
        (len_file_name + len_extra_data) as _,
    ))?;
    Ok(())
}

impl<'a> ZipArchive<'a> {
    pub(super) fn new(
        file: OwnedFile<'a>,
        non_unicode_compat: &ArchiveNonUnicodeCompatConfig,
    ) -> Result<Self, (OwnedFile<'a>, FileSystemError)> {
        use std::collections::btree_map::Entry::*;

        fn check_end_of_central_dir_record(
            record: &ZipEndOfCentralDirRecord,
        ) -> anyhow::Result<()> {
            if record.num_disk != record.num_disk_central_dir_start {
                anyhow::bail!("multi-disk zip archive not supported");
            }
            if record.total_central_dir_records_this_disk != record.total_central_dir_records {
                anyhow::bail!("multi-disk zip archive not supported");
            }
            if record.num_disk == 0xffff {
                anyhow::bail!("Zip64 archive not supported")
            }
            Ok(())
        }

        fn build_file_tree(
            cd: Vec<ZipCentralDirRecord>,
            root_index: u64,
            non_unicode_compat: &ArchiveNonUnicodeCompatConfig,
        ) -> anyhow::Result<ZipFolderEntry> {
            let mut cd_tree = BTreeMap::new();
            // TODO: Check for Unicode flag

            let encoding = match &non_unicode_compat.encoding_override {
                ArchiveNonUnicodeEncoding::AutoDetect => {
                    let mut detector = chardetng::EncodingDetector::new();
                    // NOTE: Some archives are mixing UTF-8 and non-UTF-8 entries together,
                    //       so we need to filter out UTF-8 ones
                    for record in cd
                        .iter()
                        .filter(|x| simdutf8::basic::from_utf8(&x.file_name).is_err())
                    {
                        detector.feed(&record.file_name, false);
                    }
                    detector.feed(&[], true);
                    let encoding = detector.guess(None, false);

                    // log::debug!("Guessing encoding: picked {encoding:?}");

                    ArchiveNonUnicodeEncoding::Specified(encoding.name().to_owned())
                }
                x => x.clone(),
            };
            let converter = super::EncodingConverter::new(&encoding);

            // NOTE: Used for "unique" index generation
            let mut counter: u64 = 0;

            for record in cd {
                let has_utf8_flag = (record.general_purpose_bitflag & ZIP_GPFLAGS_EFS) != 0;
                let path = if has_utf8_flag && !non_unicode_compat.ignore_utf8_flags {
                    String::from_utf8_lossy(&record.file_name)
                } else {
                    if non_unicode_compat.allow_utf8_mix {
                        simdutf8::basic::from_utf8(&record.file_name)
                            .map(|x| Cow::Borrowed(x))
                            .unwrap_or_else(|_| converter.convert(&record.file_name))
                    } else {
                        converter.convert(&record.file_name)
                    }
                };

                // log::debug!("Filename: {:?} -> `{path}`", &record.file_name);

                if path.contains('\0') || path.starts_with("/") {
                    // Bad file name
                    continue;
                }
                if path.contains("../") || path.contains("./") {
                    // TODO: Sanitize path
                    continue;
                }
                if path.ends_with('/') {
                    // TODO: Handle directory files (S_IFDIR?)
                    continue;
                }

                // Insert file
                // SAFETY: Path is checked to contain no nul bytes
                let path = unsafe { SegPath::new_unchecked(&path, PathDelimiter::Slash) };
                let mut root_dir = &mut cd_tree;
                let mut iter = path.iter().peekable();
                let mut filename = "";
                while let Some(path) = iter.next() {
                    if iter.peek().is_none() {
                        filename = path;
                        break;
                    }

                    counter += 1;

                    let key = CaselessString::new(path.to_owned());
                    root_dir = match root_dir.entry(key) {
                        Occupied(e) => match e.into_mut() {
                            ZipEntry::File(_) => {
                                anyhow::bail!("file name collides with folder in zip archive")
                            }
                            ZipEntry::Folder(e) => &mut e.children,
                        },
                        Vacant(e) => match e.insert(ZipEntry::Folder(ZipFolderEntry {
                            children: BTreeMap::new(),
                            index: calculate_hash(&(root_index, counter)),
                        })) {
                            ZipEntry::Folder(e) => &mut e.children,
                            _ => unreachable!(),
                        },
                    };
                }
                if filename == "" {
                    // Bad file name
                    continue;
                }
                let key = CaselessString::new(filename.to_owned());
                match root_dir.entry(key) {
                    Occupied(_) => anyhow::bail!("file name collides in zip archive"),
                    Vacant(e) => e.insert(ZipEntry::File(ZipFileEntry {
                        index: calculate_hash(&(root_index, record.uncompressed_data_crc32)),
                        data: record,
                    })),
                };
            }

            Ok(ZipFolderEntry {
                children: cd_tree,
                index: root_index,
            })
        }

        let file_stat = match file.get_stat() {
            Ok(stat) => stat,
            Err(e) => return Err((file, e)),
        };

        let eocd = match parse_eocd_record(&file, file_stat.size) {
            Ok(record) => {
                if let Err(e) = check_end_of_central_dir_record(&record) {
                    return Err((file, e.into()));
                }
                record
            }
            Err(e) => return Err((file, e)),
        };

        let cd = match parse_central_dir_record_list(&file, &eocd) {
            Ok(record) => record,
            Err(e) => return Err((file, e)),
        };

        let cd_tree = match build_file_tree(cd, file_stat.index, non_unicode_compat) {
            Ok(tree) => tree,
            Err(e) => return Err((file, e.into())),
        };

        Ok(ZipArchive {
            file,
            file_index: file_stat.index,
            eocd,
            cd: cd_tree,
        })
    }
}

impl ZipArchive<'_> {
    fn resolve_path<'a, 's>(
        &'a self,
        path: SegPath<'s>,
    ) -> FileSystemResult<(Option<&'a ZipFolderEntry>, &'s str)> {
        let mut parent: Option<&ZipFolderEntry> = None;
        let mut cur_dir = &self.cd;
        let mut it = path.into_iter().peekable();
        let mut non_empty = false;
        let mut filename = "";
        while let Some(path) = it.next() {
            non_empty = true;
            if let None = it.peek() {
                // Find in last level
                filename = path;
                break;
            }
            // Find the next folder
            let next_dir = if let Some(ZipEntry::Folder(folder)) =
                cur_dir.children.get(CaselessStr::new(path))
            {
                folder
            } else {
                return Err(FileSystemError::ObjectPathNotFound);
            };
            cur_dir = next_dir;
        }
        if non_empty {
            parent = Some(cur_dir);
        }
        Ok((parent, filename))
    }
}

impl super::ArchiveHandler for ZipArchive<'_> {
    fn open_file(
        &self,
        filename: SegPath,
    ) -> FileSystemResult<super::ArchiveHandlerOpenFileInfo<'_>> {
        // log::debug!("zip: Opening `{}`...", filename.get_path());

        let (parent, filename) = self.resolve_path(filename)?;

        let entry = if let Some(parent) = parent {
            parent
                .children
                .get(CaselessStr::new(filename))
                .ok_or(FileSystemError::ObjectNameNotFound)?
                .as_borrowed()
        } else {
            BorrowedZipEntry::Folder(&self.cd)
        };

        // Parse local header
        let data_reader = match entry {
            BorrowedZipEntry::File(e) => {
                if (e.data.general_purpose_bitflag & ZIP_GPFLAGS_ENCRYPTED) != 0 {
                    // TODO: Support entrypted archives
                    return Err(FileSystemError::FileCorruptError);
                }
                let mut cursor_file =
                    CursorFile::with_position(&self.file, e.data.local_file_header_offset as _);
                // let local_record = parse_local_file_record(&mut cursor_file)?;
                skip_local_file_record_with_central_no_verify(&mut cursor_file, &e.data)?;
                let data_start = cursor_file.get_position();
                match e.data.compression_method {
                    ZIP_COMPRESSION_STORE => ZipFileReader::Store(ZipFileStoreReader {
                        start: data_start,
                        size: e.data.uncompressed_size as _,
                    }),
                    ZIP_COMPRESSION_DEFLATE => {
                        // Decompression is very expensive, so delay the operation
                        ZipFileReader::Deflate(ZipFileDeflateReader {
                            start: data_start,
                            data: RwLock::new(None),
                        })
                    }
                    _ => {
                        log::warn!("zip: unsupported compression method");
                        return Err(FileSystemError::FileCorruptError);
                    }
                }
            }
            BorrowedZipEntry::Folder(e) => ZipFileReader::Null,
        };

        Ok(super::ArchiveHandlerOpenFileInfo {
            context: Box::new(ZipFile {
                root: self,
                entry,
                reader: data_reader,
            }),
            is_dir: entry.is_dir(),
        })
    }
}

struct ZipFileStoreReader {
    start: u64,
    size: u64,
}

struct ZipFileDeflateReader {
    start: u64,
    data: RwLock<Option<Vec<u8>>>,
}

enum ZipFileReader {
    Null,
    Store(ZipFileStoreReader),
    Deflate(ZipFileDeflateReader),
}

pub struct ZipFile<'a> {
    root: &'a ZipArchive<'a>,
    // path: &'a CaselessStr,
    // is_dir: bool,
    // entry: Option<&'a ZipEntry>,
    entry: BorrowedZipEntry<'a>,
    reader: ZipFileReader,
}

// impl<'a> ZipFile<'a> {
//     fn new(root: &'a ZipArchive<'a>, path: &'a CaselessStr) -> Self {
//         Self { root, path }
//     }
// }

// impl<'a> ZipFile<'a> {
//     fn handle_entry<T>(&self, func: impl FnOnce(&'a ZipEntry) -> T) -> T {
//         match self.entry {
//             Some(e) => func(e),
//             None => func(&ZipEntry::Folder(self.root.cd)),
//         }
//         // func()
//     }
// }

impl super::ArchiveFile for ZipFile<'_> {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> FileSystemResult<u64> {
        let entry = match self.entry {
            BorrowedZipEntry::File(e) => e,
            BorrowedZipEntry::Folder(_) => return Err(FileSystemError::FileIsADirectory),
        };
        let file = &self.root.file;
        match &self.reader {
            // We already handled the directory case, so return FileCorruptError here
            ZipFileReader::Null => Err(FileSystemError::FileCorruptError),
            ZipFileReader::Store(r) => {
                let read_len = buffer.len().min(r.size as _);
                file.read_at(r.start + offset, &mut buffer[..read_len])
            }
            ZipFileReader::Deflate(r) => {
                let data = loop {
                    {
                        let guard = r.data.read().unwrap();
                        if guard.is_some() {
                            break guard;
                        }
                    }

                    // Actually start reading file content
                    *r.data.write().unwrap() = Some({
                        // TODO: Use own deflate implementation to support better random access,
                        //       so that we just need ~32KB for every file handle
                        let mut cursor_file = CursorFile::with_position(&self.root.file, r.start);
                        let source_len = entry.data.compressed_size as _;
                        let mut source = Vec::with_capacity(source_len);
                        unsafe {
                            cursor_file
                                .read_exact(std::slice::from_raw_parts_mut(
                                    source.as_mut_ptr(),
                                    source_len,
                                ))
                                .map_err(anyhow::Error::from)?;
                            source.set_len(source_len);
                        }
                        let data_len = entry.data.uncompressed_size as _;
                        let mut data = Vec::with_capacity(data_len);
                        let mut decompressor = libdeflater::Decompressor::new();
                        unsafe {
                            decompressor
                                .deflate_decompress(
                                    &source,
                                    std::slice::from_raw_parts_mut(data.as_mut_ptr(), data_len),
                                )
                                .map_err(anyhow::Error::from)?;
                            data.set_len(data_len);
                        }
                        data
                    });
                    break r.data.read().unwrap();
                };
                // SAFETY: Reader is already initialized
                let data = unsafe { data.as_ref().unwrap_unchecked() };

                if offset as usize >= data.len() {
                    Ok(0)
                } else {
                    let mut src = &data[offset as _..];
                    src.read(buffer)
                        .map(|x| x as _)
                        .map_err(|e| FileSystemError::Other(e.into()))
                }
            }
        }
    }
    fn get_stat(&self) -> FileSystemResult<FileStatInfo> {
        Ok(self.entry.get_file_stat_info())
    }
    fn find_files_with_pattern(
        &self,
        pattern: &dyn crate::fs_provider::FilePattern,
        filler: &mut dyn crate::fs_provider::FindFilesDataFiller,
    ) -> FileSystemResult<()> {
        let entry = match self.entry {
            BorrowedZipEntry::Folder(e) => e,
            BorrowedZipEntry::File(_) => return Err(FileSystemError::NotADirectory),
        };
        for (name, child) in entry
            .children
            .iter()
            .filter(|(name, _)| pattern.check_name(name.as_str()))
        {
            if filler
                .fill_data(name.as_str(), &child.get_file_stat_info())
                .is_err()
            {
                log::warn!("Failed to fill object data");
            }
        }
        Ok(())
    }
}
