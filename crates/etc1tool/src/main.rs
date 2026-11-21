use std::path::Path;
use std::process::ExitCode;

use etc1tool::{decode_file, default_output_path, encode_file};

fn print_usage(prog: &str) {
    eprintln!(
        "\
{prog} infile [--help | --encode | --encodeNoHeader | --decode] [--showDifference difffile] [-o outfile]
\tDefault is --encode
\t\t--help           print this usage information.
\t\t--encode         create an ETC1 file from a PNG file.
\t\t--encodeNoHeader create a raw ETC1 data file (without a header) from a PNG file.
\t\t--decode         create a PNG file from an ETC1 file.
\t\t--showDifference difffile    Write difference between original and encoded
\t\t                             image to difffile. (Only valid when encoding).
\tIf outfile is not specified, an outfile path is constructed from infile,
\twith the apropriate suffix (.pkm or .png)."
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Encode,
    EncodeNoHeader,
    Decode,
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("etc1tool");
    if args.len() < 2 {
        print_usage(prog);
        return 255;
    }

    let mut infile: Option<String> = None;
    let mut outfile: Option<String> = None;
    let mut difffile: Option<String> = None;
    let mut mode: Option<Mode> = None;

    let mut i = 1usize;
    while i < args.len() {
        let arg = &args[i];
        if arg.starts_with('-') {
            if arg.starts_with("--") {
                match arg.as_str() {
                    "--help" => {
                        print_usage(prog);
                        return 0;
                    }
                    "--encode" => {
                        if mode.is_some() {
                            eprintln!(
                                "At most one occurrence of --encode --encodeNoHeader or --decode is allowed."
                            );
                            print_usage(prog);
                            return 255;
                        }
                        mode = Some(Mode::Encode);
                    }
                    "--encodeNoHeader" => {
                        if mode.is_some() {
                            eprintln!(
                                "At most one occurrence of --encode --encodeNoHeader or --decode is allowed."
                            );
                            print_usage(prog);
                            return 255;
                        }
                        mode = Some(Mode::EncodeNoHeader);
                    }
                    "--decode" => {
                        if mode.is_some() {
                            eprintln!(
                                "At most one occurrence of --encode --encodeNoHeader or --decode is allowed."
                            );
                            print_usage(prog);
                            return 255;
                        }
                        mode = Some(Mode::Decode);
                    }
                    "--showDifference" => {
                        if difffile.is_some() {
                            eprintln!("Only one --showDifference option allowed.");
                            print_usage(prog);
                            return 255;
                        }
                        i += 1;
                        if i >= args.len() {
                            eprintln!("Expected difffile after --showDifference");
                            print_usage(prog);
                            return 255;
                        }
                        difffile = Some(args[i].clone());
                    }
                    _ => {
                        eprintln!("Unknown flag {arg}");
                        print_usage(prog);
                        return 255;
                    }
                }
            } else if arg == "-o" {
                if outfile.is_some() {
                    eprintln!("Only one -o option allowed.");
                    print_usage(prog);
                    return 255;
                }
                i += 1;
                if i >= args.len() {
                    eprintln!("Expected outfile after -o");
                    print_usage(prog);
                    return 255;
                }
                outfile = Some(args[i].clone());
            } else {
                eprintln!("Unknown flag {arg}");
                print_usage(prog);
                return 255;
            }
        } else {
            if let Some(prev) = &infile {
                eprintln!("Already have input file {prev}, don't know what to do with {arg}");
                print_usage(prog);
                return 255;
            }
            infile = Some(arg.clone());
        }
        i += 1;
    }

    let selected_mode = mode.unwrap_or(Mode::Encode);
    let is_encode = matches!(selected_mode, Mode::Encode | Mode::EncodeNoHeader);
    if !is_encode && difffile.is_some() {
        eprintln!("--showDifference is only valid when encoding.");
        print_usage(prog);
        return 255;
    }

    let Some(in_path_str) = infile else {
        print_usage(prog);
        return 255;
    };

    let out_path_str = match outfile {
        Some(p) => p,
        None => {
            let ext = if is_encode { ".pkm" } else { ".png" };
            match default_output_path(&in_path_str, ext) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{e}");
                    return 255;
                }
            }
        }
    };

    let res = match selected_mode {
        Mode::Encode => encode_file(
            Path::new(&in_path_str),
            Path::new(&out_path_str),
            true,
            difffile.as_deref().map(Path::new),
        ),
        Mode::EncodeNoHeader => encode_file(
            Path::new(&in_path_str),
            Path::new(&out_path_str),
            false,
            difffile.as_deref().map(Path::new),
        ),
        Mode::Decode => decode_file(Path::new(&in_path_str), Path::new(&out_path_str)),
    };

    if let Err(e) = res {
        eprintln!("{e}");
        return 1;
    }
    0
}

fn main() -> ExitCode {
    ExitCode::from(run() as u8)
}
