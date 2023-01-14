#![feature(fs_try_exists)]

use std::{
    fs::{File, OpenOptions},
    io::{copy, BufReader},
    path::PathBuf,
    slice::Iter,
};

use anyhow::{anyhow, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use zip::read::read_zipfile_from_stream;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg()]
    chromium_version: String,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {:?}", err);
    }
}

fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    // history.json 包含了 base_position 和版本号，根据用户提供的版本号，找到一个 base_position。
    let history_list = get_history_list()?;
    let matched_history_list = find_history(history_list.iter(), &args.chromium_version);
    let mut chromium_base_position = None;
    for history in matched_history_list {
        let deps = fetch_deps(&history.version)?;
        if let Some(pos) = deps.chromium_base_position {
            match pos.parse::<usize>() {
                Ok(pos) => {
                    chromium_base_position = Some(pos);
                    break;
                }
                Err(err) => println!(
                    "==> chromium {}: parse base_position error: {:?}",
                    deps.chromium_version, err
                ),
            }
        } else {
            println!(
                "==> chromium {}: no chromium_base_position.",
                deps.chromium_version
            );
        }
    }
    let chromium_base_position =
        chromium_base_position.ok_or_else(|| anyhow!("未能根据版本号找到 position。"))?;

    // 然后遍历所有的版本信息，取得最近的可以下载的 position 的 prefix。
    let builds = get_build_list()?;
    let (prefix, revision) = find_builds(builds.iter(), chromium_base_position)
        .ok_or_else(|| anyhow::anyhow!("未找到 position <= {} 的版本。", chromium_base_position))?;
    println!("==> found nearest revision: {}", revision);

    // 根据 prefix 找到该版本文件列表，以及 chrome-win.zip 文件信息。
    let build_files = fetch_build_detail(prefix)?;
    let win_zip = build_files
        .iter()
        .find(|f| f.name.ends_with("chrome-win.zip"))
        .or_else(|| {
            build_files
                .iter()
                .find(|f| f.name.ends_with("chrome-win32.zip"))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "在版本 {} 中，未找到 chrome-win.zip/chrome-win32-zip。",
                prefix
            )
        })?;

    // 开始下载压缩文件。
    println!("==> downloading {}", win_zip.media_link);
    let mut win_zip_response = reqwest::blocking::get(&win_zip.media_link)?;

    // 先保存到临时目录里面，待解压的时候，找到里面的版本信息，再重命名一下文件夹。
    let base_path = std::env::current_dir()?.join(format!("tmp-chromium-{}", revision));
    std::fs::create_dir_all(&base_path)?;

    // 执行 zip 解压过程，并去除压缩包的根目录。
    let mut prefix = String::new();
    let mut version_list = Vec::new();
    loop {
        let mut zip = match read_zipfile_from_stream(&mut win_zip_response) {
            Ok(Some(zip)) => zip,
            Ok(None) => break,
            Err(err) => panic!("Error: {:?}", err),
        };

        let zip_name = zip.name();
        if prefix.is_empty() {
            if zip.is_dir() {
                prefix = zip.name().to_owned();
            } else {
                panic!("Invalid zip file");
            }
        } else if zip_name.starts_with(&prefix) {
            const MANIFEST_SUFFIX: &str = ".manifest";
            if zip_name.ends_with(MANIFEST_SUFFIX) {
                let manifest_name = zip_name
                    .rsplit_once('/')
                    .map(|(_, n)| n)
                    .unwrap_or(zip_name);
                let manifest_name =
                    manifest_name[..manifest_name.len() - MANIFEST_SUFFIX.len()].to_owned();
                if manifest_name
                    .split('.')
                    .into_iter()
                    .all(|part| part.parse::<usize>().is_ok())
                {
                    version_list.push(manifest_name);
                }
            }
            let file_path = base_path.join(&zip_name[prefix.len()..]);
            if zip.is_dir() {
                std::fs::create_dir_all(&file_path).map_err(|err| {
                    anyhow!(
                        "创建目录 {} 时出错：{:?}",
                        file_path.to_str().unwrap_or_default(),
                        err
                    )
                })?;
            } else {
                copy(
                    &mut zip,
                    &mut OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .create(true)
                        .open(&file_path)
                        .map_err(|err| {
                            anyhow!(
                                "解压文件 {} 时出错：{:?}",
                                file_path.to_str().unwrap_or_default(),
                                err
                            )
                        })?,
                )
                .map_err(|err| {
                    anyhow!(
                        "解压文件 {} 时出错：{:?}",
                        file_path.to_str().unwrap_or_default(),
                        err
                    )
                })?;
            }
        } else {
            panic!("Invalid file name");
        }

        println!("==> unzip: {}", zip.name());
    }

    // 有些 chrome 压缩包内可能存在多个形如“版本号.manifest”的文件，这里是找到最新的一个版本号，然后作为最终目录名。
    let version = find_latest_version(&version_list)
        .map(|(major, minor, branch, patch)| format!("{major}.{minor}.{branch}.{patch}"))
        .unwrap_or_else(|| revision.to_string());
    let dest_path = std::env::current_dir()?.join(format!("chromium-{}", version));
    println!(
        "==> moving {} to {}",
        base_path.to_str().unwrap_or_default(),
        dest_path.to_str().unwrap_or_default()
    );
    std::fs::rename(base_path, dest_path).map_err(|err| anyhow!("移动目录出错：{:?}", err))?;

    Ok(())
}

fn find_latest_version(version_list: &[String]) -> Option<(usize, usize, usize, usize)> {
    let mut latest_version = None;
    version_list.iter().for_each(|ver| {
        let split = ver.split('.');
        if let &[major, minor, branch, patch] = split
            .filter_map(|v| v.parse::<usize>().ok())
            .collect::<Vec<_>>()
            .as_slice()
        {
            let ver_tuple = (major, minor, branch, patch);
            if let Some(ver) = &latest_version {
                if ver_tuple > *ver {
                    latest_version = Some(ver_tuple);
                }
            } else {
                latest_version = Some(ver_tuple);
            }
        }
    });
    latest_version
}

fn get_build_list() -> Result<Vec<String>> {
    let builds_json_path = get_cached_file_path("builds.json")?;
    if std::fs::try_exists(&builds_json_path).unwrap_or_default() {
        println!("==> using cached builds.");
        Ok(serde_json::from_reader(BufReader::new(File::open(
            &builds_json_path,
        )?))?)
    } else {
        println!("==> retrieving builds ...");
        let pages = ChromiumBuildsPage::new();
        let mut unwrapped_page_list = Vec::new();
        for page in pages {
            unwrapped_page_list.push(page?);
        }
        let builds: Vec<String> = unwrapped_page_list.into_iter().flatten().collect();
        std::fs::write(&builds_json_path, serde_json::to_string(&builds)?)?;
        Ok(builds)
    }
}

fn get_history_list() -> Result<Vec<ChromiumHistoryInfo>> {
    let history_json_path = get_cached_file_path("history.json")?;
    if std::fs::try_exists(&history_json_path).unwrap_or_default() {
        println!("==> using cached history.");
        Ok(serde_json::from_reader(BufReader::new(File::open(
            &history_json_path,
        )?))?)
    } else {
        println!("==> retrieving history.json ...");
        let url = "https://omahaproxy.appspot.com/history.json?os=win&channel=stable";
        let response = reqwest::blocking::get(url)?;
        let history_list: Vec<ChromiumHistoryInfo> = serde_json::from_reader(response)?;
        std::fs::write(&history_json_path, serde_json::to_string(&history_list)?)?;
        Ok(history_list)
    }
}

fn get_cached_file_path(file: &str) -> Result<PathBuf> {
    let mut path = PathBuf::new();
    path.push(std::env::var("LOCALAPPDATA")?);
    path.push("fetchchromium");
    if !path.exists() {
        std::fs::create_dir_all(&path)?;
    }
    path.push(file);
    Ok(path)
}

fn fetch_build_detail(prefix: &str) -> Result<Vec<GoogleApiStorageObject>> {
    let url = format!("https://www.googleapis.com/storage/v1/b/chromium-browser-snapshots/o?delimiter=/&prefix={prefix}&fields=items(kind,mediaLink,metadata,name,size,updated),kind,prefixes,nextPageToken");
    println!("==> fetching history {} ...", url);
    let response = reqwest::blocking::get(url)?;
    let build_detail: ChromiumBuildPage = serde_json::from_reader(response)?;
    Ok(build_detail.items)
}

fn fetch_deps(version: &str) -> Result<ChromiumDepsInfo> {
    let url = format!("https://omahaproxy.appspot.com/deps.json?version={version}");
    println!("==> fetching deps {} ...", url);
    let response = reqwest::blocking::get(url)?;
    Ok(serde_json::from_reader(response)?)
}

fn find_history<'a>(
    history_list: Iter<'a, ChromiumHistoryInfo>,
    ver: &str,
) -> Vec<&'a ChromiumHistoryInfo> {
    let prefix = format!("{}.", ver);
    history_list
        .filter(|info| info.version.starts_with(&prefix))
        .collect()
}

fn find_builds(build_list: Iter<String>, find_pos: usize) -> Option<(&String, usize)> {
    let prefix = "Win_x64/";
    let mut list: Vec<_> = build_list
        .filter_map(|build| {
            if build.starts_with(prefix) {
                if let Ok(build_pos) = build[prefix.len()..build.len() - 1].parse::<usize>() {
                    Some((build, build_pos))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();
    list.sort_by(|a, b| a.1.cmp(&b.1));
    list.into_iter().rev().find(|build| build.1 <= find_pos)
}

pub(crate) struct ChromiumBuildsPage {
    next_page_token: Option<String>,
    done: bool,
}

impl ChromiumBuildsPage {
    pub fn new() -> Self {
        Self {
            next_page_token: None,
            done: false,
        }
    }
}

impl Iterator for ChromiumBuildsPage {
    type Item = Result<Vec<String>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            None
        } else {
            let next_page_token = self
                .next_page_token
                .as_ref()
                .map(|t| format!("&pageToken={}", t))
                .unwrap_or_default();
            let url = format!("https://www.googleapis.com/storage/v1/b/chromium-browser-snapshots/o?delimiter=/&prefix=Win_x64/&fields=items(kind,mediaLink,metadata,name,size,updated),kind,prefixes,nextPageToken{}", next_page_token);

            let prefixes = reqwest::blocking::get(&url)
                .map_err(|err| anyhow!("请求 {} 时出错：{:?}", url, err))
                .and_then(|response| {
                    let page: ChromiumBuildPage = serde_json::from_reader(response)?;
                    self.next_page_token = page.next_page_token;
                    self.done = self.next_page_token.is_none();
                    Ok(page.prefixes)
                });

            prefixes
                .map(|p| if p.is_empty() { None } else { Some(Ok(p)) })
                .unwrap_or_else(|e| Some(Err(e)))
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChromiumBuildPage {
    kind: String,
    next_page_token: Option<String>,
    #[serde(default)]
    prefixes: Vec<String>,
    #[serde(default)]
    items: Vec<GoogleApiStorageObject>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GoogleApiStorageObject {
    kind: String,
    media_link: String,
    name: String,
    size: String,
    updated: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ChromiumReleaseInfo {
    channel: String,
    chromium_main_branch_position: Option<usize>,
    milestone: usize,
    platform: String,
    previous_version: String,
    time: usize,
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ChromiumHistoryInfo {
    channel: String,
    os: String,
    timestamp: String,
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ChromiumDepsInfo {
    chromium_base_commit: Option<String>,
    chromium_base_position: Option<String>,
    chromium_branch: Option<String>,
    chromium_commit: String,
    chromium_version: String,
    skia_commit: String,
    v8_commit: String,
    v8_position: String,
    v8_version: String,
}
