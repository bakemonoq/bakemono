use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use bakemono_scraper::{Cookies, ScrapeRequest, Scraper};

fn main() -> Result<()> {
    let request = parse_args()?;
    let scraper = match std::env::var_os("BAKEMONO_GALLERY_DL") {
        Some(path) => Scraper::with_binary(path),
        None => Scraper::new(),
    };
    let version = scraper
        .version()
        .context("gallery-dl not found, install it with `pipx install gallery-dl`")?;
    eprintln!("using {version}");

    let outcome = scraper.scrape(&request)?;
    println!(
        "scraped {} files for {} into {}",
        outcome.files.len(),
        outcome.creator,
        outcome.dest.display()
    );
    for file in &outcome.files {
        println!("  {} ({} bytes)", file.path.display(), file.size);
    }
    Ok(())
}

fn parse_args() -> Result<ScrapeRequest> {
    let mut creator = None;
    let mut dest = None;
    let mut cookies = None;
    let mut limit = None;
    let mut quiet = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cookies" => cookies = Some(Cookies::File(value(&mut args, "--cookies")?.into())),
            "--browser" => cookies = Some(Cookies::Browser(value(&mut args, "--browser")?)),
            "--limit" => {
                limit = Some(
                    value(&mut args, "--limit")?
                        .parse()
                        .context("--limit expects a number")?,
                )
            }
            "-q" | "--quiet" => quiet = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            flag if flag.starts_with('-') => bail!("unknown flag {flag}"),
            positional if creator.is_none() => creator = Some(positional.to_string()),
            positional if dest.is_none() => dest = Some(PathBuf::from(positional)),
            other => bail!("unexpected argument {other}"),
        }
    }

    let creator = creator.context(USAGE)?;
    let mut request = ScrapeRequest::new(creator, dest.unwrap_or_else(|| PathBuf::from("scrape")));
    request.cookies = cookies.or_else(cookies_from_env);
    request.limit = limit;
    request.quiet = quiet;
    Ok(request)
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .with_context(|| format!("{flag} expects a value"))
}

fn cookies_from_env() -> Option<Cookies> {
    if let Some(path) = std::env::var_os("BAKEMONO_COOKIES") {
        Some(Cookies::File(PathBuf::from(path)))
    } else {
        std::env::var("BAKEMONO_COOKIES_BROWSER")
            .ok()
            .map(Cookies::Browser)
    }
}

fn print_usage() {
    eprintln!("{USAGE}");
    eprintln!(
        "env: BAKEMONO_COOKIES=<file>, BAKEMONO_COOKIES_BROWSER=<name>, BAKEMONO_GALLERY_DL=<path>"
    );
}

const USAGE: &str =
    "usage: bakemono-scraper <creator> [dest] [--cookies FILE | --browser NAME] [--limit N] [--quiet]";
