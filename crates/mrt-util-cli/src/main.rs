//! I need a way to debug/mainpulate .mrt files to write tests/do debugging. This crate is just a
//! small CLI wrapper around [bgpkit-parser](https://github.com/bgpkit/bgpkit-parser) to let me
//! do that. It's not very general purpose, and not deserving of its own repo

use anyhow::anyhow;
use bgpkit_parser::{BgpElem, BgpkitParser};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rand::prelude::IteratorRandom;
use rand::rngs::SmallRng;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write, stdout};
use std::path::PathBuf;

#[derive(Debug, Parser)]
struct Cli {
    #[arg(help = "File to read. Supports anything [oneio](https://github.com/bgpkit/oneio) does")]
    in_file: String,
    #[command(subcommand)]
    subcommand: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Cat,
    Head(HeadArgs),
    CountPrefixes,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
enum Format {
    Mrt,
    Json,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
enum Compression {
    None,
    Gz,
}

#[derive(Debug, Args)]
struct HeadArgs {
    #[arg(
        help = "Output to this file. If omitted, write to stdout. Compression is inferred by file \
                suffix (.mrt for no compression)"
    )]
    out_file: Option<PathBuf>,
    #[arg(
        long,
        short = 'n',
        help = "Output <count> records and then stop",
        default_value = "10"
    )]
    count: usize,
    #[arg(value_enum, short = 'F', default_value = "mrt")]
    format: Format,
    #[arg(value_enum, short = 'c', default_value = "none")]
    compression: Compression,
    #[arg(long, help = "Pick a random sample instead of the first values")]
    random: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::try_parse()?;
    let parser = open_file(&cli.in_file)?;

    match cli.subcommand {
        Command::Head(args) => {
            let records = parser.into_iter();
            let records: Box<dyn Iterator<Item = BgpElem>> = if args.random {
                let mut rng: SmallRng = rand::make_rng();
                Box::new(records.sample(&mut rng, args.count).into_iter())
            } else {
                Box::new(records.take(args.count))
            };

            let mut output = open_output(&args.out_file, args.compression)?;

            match args.format {
                Format::Mrt => {
                    write_mrt(&mut output, records)?;
                }
                Format::Json => {
                    write_json(&mut output, records)?;
                }
            }
        }
        Command::Cat => {
            let records = parser.into_iter();
            let mut output = open_output(&None, Compression::None)?;
            write_json(&mut output, records)?;
        }
        Command::CountPrefixes => {
            let mut counter = HashMap::new();
            for record in parser {
                *counter.entry(record.prefix).or_insert(0usize) += 1;
            }
            let mut top_counts = counter.iter().collect::<Vec<_>>();
            top_counts.sort_by_key(|(_, v)| **v);
            top_counts.reverse();

            let mut count_counter = HashMap::new();
            for &count in counter.values() {
                *count_counter.entry(count).or_insert(0) += 1usize;
            }

            for (prefix, count) in top_counts {
                println!("{prefix}: {count:5}");
            }

            println!("### Count frequencies");
            let mut frequencies = count_counter.iter().collect::<Vec<_>>();
            frequencies.sort_by_key(|(_, v)| -(**v as isize));

            for (count, frequency) in frequencies {
                println!("{count}: {frequency}");
            }

            println!("Unique count: {}", counter.len());
        }
    }

    Ok(())
}

fn open_output(
    out_path: &Option<PathBuf>,
    compression: Compression,
) -> anyhow::Result<Box<dyn Write>> {
    let out_file: BufWriter<Box<dyn Write>> = if let Some(out_path) = out_path {
        let file = File::create(out_path)?;
        BufWriter::new(Box::new(file))
    } else {
        BufWriter::new(Box::new(stdout()))
    };

    let writer: Box<dyn Write> = match compression {
        Compression::None => Box::new(out_file),
        Compression::Gz => Box::new(flate2::write::GzEncoder::new(
            out_file,
            flate2::Compression::new(9),
        )),
    };
    Ok(writer)
}

fn write_json(
    out_file: &mut impl Write,
    records: impl IntoIterator<Item = BgpElem>,
) -> anyhow::Result<()> {
    serde_json::to_writer_pretty(out_file, &records.into_iter().collect::<Vec<_>>())
        .map_err(|e| anyhow!(e))
}

fn write_mrt(
    out_file: &mut impl Write,
    records: impl IntoIterator<Item = BgpElem>,
) -> anyhow::Result<()> {
    let mut encoder = bgpkit_parser::encoder::MrtRibEncoder::new();

    for record in records {
        encoder.process_elem(&record);
    }

    out_file.write_all(encoder.export_bytes().as_ref())?;
    Ok(())
}

fn open_file(path: &str) -> anyhow::Result<BgpkitParser<Box<dyn Read + Send>>> {
    let buf_reader = BufReader::new(File::open(path)?);
    let reader: Box<dyn Read + Send> = if path.ends_with(".gz") {
        Box::new(flate2::bufread::GzDecoder::new(buf_reader))
    } else {
        Box::new(buf_reader)
    };
    Ok(BgpkitParser::from_reader(reader))
}
