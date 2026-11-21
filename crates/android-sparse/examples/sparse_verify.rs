use std::fs::File;
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use android_sparse::{
    is_sparse_image, split_sparse, unsparse, ParsedChunk, SparseImage, SparseReader,
};

fn usage() -> ! {
    eprintln!("Usage:");
    eprintln!("  sparse_verify unsparse <in.simg> <out.img>");
    eprintln!("  sparse_verify roundtrip-split <in.simg> <max_part_bytes> <out_reassembled.img>");
    std::process::exit(2);
}

fn write_split_part(part: &android_sparse::SplitPart, src_file: &mut File) -> Vec<u8> {
    let mut out = Vec::with_capacity(part.sparse_size() as usize);
    out.extend_from_slice(&part.header.to_bytes());
    let mut buf = Vec::new();
    for c in &part.chunks {
        out.extend_from_slice(&c.header.to_bytes());
        if c.data_size > 0 {
            src_file.seek(SeekFrom::Start(c.input_offset)).unwrap();
            buf.resize(c.data_size as usize, 0);
            src_file.read_exact(&mut buf).unwrap();
            out.extend_from_slice(&buf);
        }
    }
    out
}

fn cmd_unsparse(in_path: &Path, out_path: &Path) {
    let mut f = BufReader::new(File::open(in_path).unwrap());
    assert!(is_sparse_image(&mut f).unwrap(), "expected sparse image");
    let mut out = File::create(out_path).unwrap();
    unsparse(f, &mut out, true).unwrap();
}

fn cmd_roundtrip_split(in_path: &Path, max_part: u64, out_path: &Path) {
    let f = BufReader::new(File::open(in_path).unwrap());
    let mut sr = SparseReader::new(f, true).unwrap();
    let header = sr.header().clone();
    let mut chunks = Vec::new();
    while let Some(c) = sr.next_chunk().unwrap() {
        match c {
            ParsedChunk::Raw { header, .. } => {
                sr.skip_raw_data().unwrap();
                chunks.push(header);
            }
            ParsedChunk::Fill { header, .. }
            | ParsedChunk::DontCare { header }
            | ParsedChunk::Crc32 { header, .. } => chunks.push(header),
        }
    }

    let parts = split_sparse(&header, &chunks, max_part).unwrap();
    assert!(!parts.is_empty());

    let mut raw_buf = Cursor::new(vec![0u8; header.expanded_size() as usize]);
    let mut src_file = File::open(in_path).unwrap();
    for part in &parts {
        assert!(part.sparse_size() <= max_part);
        let bytes = write_split_part(part, &mut src_file);
        raw_buf.seek(SeekFrom::Start(0)).unwrap();
        unsparse(Cursor::new(&bytes), &mut raw_buf, false).unwrap();
    }
    let reassembled = raw_buf.into_inner();

    // Re-sparsify with CRC32 and unsparse with verify_crc = true.
    let mut raw_cur = Cursor::new(&reassembled);
    let simg = SparseImage::from_raw_with_options(
        &mut raw_cur,
        header.block_size,
        reassembled.len() as u64,
        true,
    )
    .unwrap();
    let mut re_sparse = Vec::new();
    simg.write_to(&mut re_sparse, true).unwrap();

    let mut out = BufWriter::new(File::create(out_path).unwrap());
    let mut verified_raw = Cursor::new(Vec::with_capacity(reassembled.len()));
    unsparse(Cursor::new(&re_sparse), &mut verified_raw, true).unwrap();
    let final_bytes = verified_raw.into_inner();
    assert_eq!(final_bytes, reassembled);
    use std::io::Write;
    out.write_all(&final_bytes).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    match args[1].as_str() {
        "unsparse" if args.len() == 4 => {
            cmd_unsparse(Path::new(&args[2]), Path::new(&args[3]));
        }
        "roundtrip-split" if args.len() == 5 => {
            let max_part: u64 = args[3].parse().unwrap();
            cmd_roundtrip_split(Path::new(&args[2]), max_part, Path::new(&args[4]));
        }
        _ => usage(),
    }
}
