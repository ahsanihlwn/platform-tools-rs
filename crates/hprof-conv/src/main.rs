use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::ExitCode;

use hprof_conv::{convert, ConvertOptions};

fn print_usage() {
    eprintln!(
        "\
Usage: hprof-conv [-z] infile outfile

  -z: exclude non-app heaps, such as Zygote

Specify '-' for either or both files to use stdin/stdout.

Copyright (C) 2009 The Android Open Source Project

This software is built from source code licensed under the Apache License,
Version 2.0 (the \"License\"). You may obtain a copy of the License at

     http://www.apache.org/licenses/LICENSE-2.0

See the associated NOTICE file for further details."
    );
}

fn run() -> Result<(), i32> {
    let args: Vec<String> = std::env::args().collect();
    let mut filter_zygote = false;
    let mut pos_start = 1usize;

    while pos_start < args.len() {
        let arg = &args[pos_start];
        if arg == "--" {
            pos_start += 1;
            break;
        }
        if arg == "-" || !arg.starts_with('-') {
            break;
        }
        for ch in arg[1..].chars() {
            if ch == 'z' {
                filter_zygote = true;
            } else {
                eprintln!("{}: invalid option -- '{}'", args[0], ch);
                print_usage();
                return Err(2);
            }
        }
        pos_start += 1;
    }

    let positional = &args[pos_start..];
    if positional.len() != 2 {
        print_usage();
        return Err(2);
    }

    let infile = &positional[0];
    let outfile = &positional[1];

    let mut reader: Box<dyn Read> = if infile == "-" {
        Box::new(BufReader::new(io::stdin().lock()))
    } else {
        match File::open(infile) {
            Ok(f) => Box::new(BufReader::new(f)),
            Err(e) => {
                eprintln!("ERROR: unable to open '{}': {e}", infile);
                return Err(1);
            }
        }
    };

    let mut writer: Box<dyn Write> = if outfile == "-" {
        Box::new(BufWriter::new(io::stdout().lock()))
    } else {
        match File::create(outfile) {
            Ok(f) => Box::new(BufWriter::new(f)),
            Err(e) => {
                eprintln!("ERROR: unable to open '{}': {e}", outfile);
                return Err(1);
            }
        }
    };

    if let Err(err) = convert(&mut reader, &mut writer, ConvertOptions { filter_zygote }) {
        eprintln!("{err}");
        return Err(1);
    }

    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code as u8),
    }
}
