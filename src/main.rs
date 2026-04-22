use clap::{ArgAction::SetTrue, Parser};
use std::{
    env,
    fmt::Display,
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// What we are looking for
    #[arg(short, long)]
    needle: String,

    /// Haystack files; the search space, by file
    #[arg(short, long, value_delimiter = ',', value_parser = parse_file_path)]
    files: Vec<PathBuf>,

    /// Let us summarize our results, rather than printing each one.
    #[arg(short, long, action = SetTrue)]
    summary: bool,
}

#[derive(Clone, Copy)]
struct Match {
    line: usize,
    col: usize,
    byte_offset: usize,
}

impl Display for Match {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Match {
            line,
            col,
            byte_offset,
        } = *self;

        write!(f, "{line}:{col} (byte {byte_offset})")
    }
}

fn expand_tilde(path: &str, home_dir: Option<&Path>) -> PathBuf {
    match path {
        "~" => home_dir.map_or_else(|| PathBuf::from(path), PathBuf::from),
        _ => path
            .strip_prefix("~/")
            .zip(home_dir)
            .map_or_else(|| PathBuf::from(path), |(rest, home)| home.join(rest)),
    }
}

fn parse_file_path(path: &str) -> Result<PathBuf, String> {
    Ok(expand_tilde(
        path,
        env::var_os("HOME").as_deref().map(Path::new),
    ))
}

fn build_lps(needle: &[u8]) -> Vec<usize> {
    let mut lps = vec![0; needle.len()];
    let mut prefix_len = 0;
    let mut i = 1;

    while i < needle.len() {
        if needle[i] == needle[prefix_len] {
            prefix_len += 1;
            lps[i] = prefix_len;
            i += 1;
        } else if prefix_len > 0 {
            prefix_len = lps[prefix_len - 1];
        } else {
            i += 1;
        }
    }

    lps
}

fn find_matches<R: BufRead>(reader: &mut R, needle: &[u8]) -> Result<Vec<Match>, std::io::Error> {
    let needle_len = needle.len();

    if needle_len == 0 {
        return Ok(Vec::new());
    }

    let lps = build_lps(needle);
    let mut matches = Vec::new();
    let mut recent = vec![
        Match {
            line: 0,
            col: 0,
            byte_offset: 0,
        };
        needle_len
    ];
    let mut matched = 0;
    let mut line = 1;
    let mut col = 1;
    let mut index = 0;

    loop {
        let consumed = {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                break;
            }

            for &byte in chunk {
                recent[index % needle_len] = Match {
                    line,
                    col,
                    byte_offset: index,
                };

                while matched > 0 && needle[matched] != byte {
                    matched = lps[matched - 1];
                }

                if needle[matched] == byte {
                    matched += 1;

                    if matched == needle_len {
                        matches.push(recent[(index + 1 - needle_len) % needle_len]);
                        matched = lps[matched - 1];
                    }
                }

                if byte == b'\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }

                index += 1;
            }

            chunk.len()
        };

        reader.consume(consumed);
    }

    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn files_flag_accepts_comma_separated_values() {
        let args = Args::try_parse_from(["minigrep", "-f", "src/main.rs,Cargo.toml", "-n", "why"])
            .unwrap();

        assert_eq!(
            args.files,
            vec![PathBuf::from("src/main.rs"), PathBuf::from("Cargo.toml")]
        );
    }

    #[test]
    fn summary_flag_defaults_to_false_and_enables_with_long_flag() {
        let args =
            Args::try_parse_from(["minigrep", "-f", "src/main.rs", "-n", "why", "--summary"])
                .unwrap();

        assert!(args.summary);

        let args = Args::try_parse_from(["minigrep", "-f", "src/main.rs", "-n", "why"]).unwrap();

        assert!(!args.summary);
    }

    #[test]
    fn expand_tilde_uses_supplied_home_directory() {
        let home = Path::new("/tmp/example-home");

        assert_eq!(
            expand_tilde("~/Downloads/t8.shakespeare.txt", Some(home)),
            home.join("Downloads/t8.shakespeare.txt")
        );
        assert_eq!(expand_tilde("~", Some(home)), home);
        assert_eq!(
            expand_tilde("src/main.rs", Some(home)),
            PathBuf::from("src/main.rs")
        );
    }

    #[test]
    fn match_display_is_concise_and_explicit() {
        let m = Match {
            line: 3,
            col: 14,
            byte_offset: 2718,
        };

        assert_eq!(m.to_string(), "3:14 (byte 2718)");
    }

    #[test]
    fn find_matches_tracks_line_column_and_byte_offset() {
        let mut reader = Cursor::new("xx\nabc\nzabc");

        let matches = find_matches(&mut reader, b"abc").unwrap();

        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 2);
        assert_eq!(matches[0].col, 1);
        assert_eq!(matches[0].byte_offset, 3);
        assert_eq!(matches[1].line, 3);
        assert_eq!(matches[1].col, 2);
        assert_eq!(matches[1].byte_offset, 8);
    }
}

fn search_file_blocking(file: PathBuf, needle: &[u8]) -> Result<Vec<Match>, std::io::Error> {
    let file = std::fs::File::open(file)?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    find_matches(&mut reader, needle)
}

async fn search_file(file: PathBuf, needle: Arc<[u8]>) -> Result<Vec<Match>, std::io::Error> {
    tokio::task::spawn_blocking(move || search_file_blocking(file, needle.as_ref()))
        .await
        .map_err(std::io::Error::other)?
}

async fn async_main() -> std::io::Result<()> {
    let Args {
        needle,
        files,
        summary,
    } = Args::parse();

    if needle.is_empty() || files.is_empty() {
        return Ok(());
    }

    let needle: Arc<[u8]> = Arc::from(needle.into_bytes());
    let tasks: Vec<_> = files
        .into_iter()
        .map(|file| {
            let needle = Arc::clone(&needle);
            tokio::spawn(async move {
                let result = search_file(file.clone(), needle).await;
                (file, result)
            })
        })
        .collect();

    let mut out = BufWriter::new(std::io::stdout());
    let mut err = BufWriter::new(std::io::stderr());

    for task in tasks {
        match task.await {
            Ok((file, Ok(matches))) => {
                if summary {
                    writeln!(out, "In {}, {} matches found", file.display(), matches.len())?
                } else {
                    for m in matches {
                        writeln!(out, "{}:{}", file.display(), m)?
                    }
                }
            }
            Ok((file, Err(e))) => writeln!(err, "{}: {}", file.display(), e)?,
            Err(e) => writeln!(err, "task failed: {}", e)?,
        }
    }

    out.flush()?;
    err.flush()?;

    Ok(())
}

pub fn main() {
    let epoch = Instant::now();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main())
        .unwrap();

    println!("Time elapsed: {:?}", epoch.elapsed());
}
