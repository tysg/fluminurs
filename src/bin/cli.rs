use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::{App, Arg};
use futures_util::{future, stream, StreamExt};
use serde::{Deserialize, Serialize};

use fluminurs::file::File;
use fluminurs::module::Module;
use fluminurs::multimedia::Video;
use fluminurs::resource::{OverwriteMode, OverwriteResult, Resource};
use fluminurs::{Api, Result};

#[macro_use]
extern crate bitflags;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

#[derive(Serialize, Deserialize)]
struct Login {
    username: String,
    password: String,
}

bitflags! {
    struct ModuleTypeFlags: u8 {
        const TAKING = 0x01;
        const TEACHING = 0x02;
    }
}

fn flush_stdout() {
    io::stdout().flush().expect("Unable to flush stdout");
}

fn get_input(prompt: &str) -> String {
    let mut input = String::new();
    print!("{}", prompt);
    flush_stdout();
    io::stdin()
        .read_line(&mut input)
        .expect("Unable to get input");
    input.trim().to_string()
}

fn get_password(prompt: &str) -> String {
    print!("{}", prompt);
    flush_stdout();
    rpassword::read_password().expect("Unable to get non-echo input mode for password")
}

async fn print_announcements(api: &Api, modules: &[Module]) -> Result<()> {
    let module_announcements = future::join_all(
        modules
            .iter()
            .map(|module| module.get_announcements(api, false)),
    )
    .await;
    for (module, announcements) in modules.iter().zip(module_announcements) {
        let announcements = announcements?;
        println!("# {} {}", module.code, module.name);
        println!();
        for ann in announcements {
            println!("=== {} ===", ann.title);
            let stripped = ammonia::Builder::new()
                .tags(HashSet::new())
                .clean(&ann.description)
                .to_string();
            let decoded = htmlescape::decode_html(&stripped)
                .unwrap_or_else(|_| "Unable to decode HTML Entities".to_owned());
            println!("{}", decoded);
        }
        println!();
        println!();
    }
    Ok(())
}

async fn load_modules_files(
    api: &Api,
    modules: &[Module],
    include_uploadable_folders: ModuleTypeFlags,
) -> Result<Vec<File>> {
    let root_dirs = modules
        .iter()
        .filter(|module| module.has_access())
        .map(|module| {
            (
                module.workbin_root(|code| Path::new(code).to_owned()),
                module.is_teaching(),
            )
        })
        .collect::<Vec<_>>();

    let (files, errors) = future::join_all(root_dirs.into_iter().map(|(root_dir, is_teaching)| {
        root_dir.load(
            api,
            include_uploadable_folders.contains(if is_teaching {
                ModuleTypeFlags::TEACHING
            } else {
                ModuleTypeFlags::TAKING
            }),
        )
    }))
    .await
    .into_iter()
    .fold((vec![], vec![]), move |(mut ok, mut err), res| {
        match res {
            Ok(mut dir) => {
                ok.append(&mut dir);
            }
            Err(e) => {
                err.push(e);
            }
        }
        (ok, err)
    });
    for e in errors {
        println!("Failed loading module files: {}", e);
    }
    Ok(files)
}

async fn load_modules_multimedia(api: &Api, modules: &[Module]) -> Result<Vec<Video>> {
    let multimedias = modules
        .iter()
        .filter(|module| module.has_access())
        .map(|module| module.multimedia_root(|code| Path::new(code).join(Path::new("Multimedia"))))
        .collect::<Vec<_>>();

    let (files, errors) = future::join_all(
        multimedias
            .into_iter()
            .map(|multimedia| multimedia.load(api)),
    )
    .await
    .into_iter()
    .fold((vec![], vec![]), move |(mut ok, mut err), res| {
        match res {
            Ok(mut dir) => {
                ok.append(&mut dir);
            }
            Err(e) => {
                err.push(e);
            }
        }
        (ok, err)
    });

    for e in errors {
        println!("Failed loading module multimedia: {}", e);
    }
    Ok(files)
}

fn list_resources<T: Resource>(resources: &[T]) {
    for resource in resources {
        println!("{}", resource.path().display())
    }
}

async fn download_resource<T: Resource>(
    api: &Api,
    file: &T,
    path: PathBuf,
    temp_path: PathBuf,
    overwrite_mode: OverwriteMode,
) {
    match file.download(api, &path, &temp_path, overwrite_mode).await {
        Ok(OverwriteResult::NewFile) => println!("Downloaded to {}", path.to_string_lossy()),
        Ok(OverwriteResult::AlreadyHave) => {}
        Ok(OverwriteResult::Skipped) => println!("Skipped {}", path.to_string_lossy()),
        Ok(OverwriteResult::Overwritten) => println!("Updated {}", path.to_string_lossy()),
        Ok(OverwriteResult::Renamed { renamed_path }) => println!(
            "Renamed {} to {}",
            path.to_string_lossy(),
            renamed_path.to_string_lossy()
        ),
        Err(e) => println!("Failed to download file: {}", e),
    }
}

async fn download_resources<T: Resource>(
    api: &Api,
    files: &[T],
    destination: &str,
    overwrite_mode: OverwriteMode,
    parallelism: usize,
) -> Result<()> {
    println!("Download to {}", destination);
    let dest_path = Path::new(destination);
    if !dest_path.is_dir() {
        return Err("Download destination does not exist or is not a directory");
    }

    stream::iter(files.iter())
        .map(|file| {
            let temp_path = dest_path
                .join(file.path().parent().unwrap())
                .join(make_temp_file_name(file.path().file_name().unwrap()));
            let real_path = dest_path.join(file.path());
            download_resource(api, file, real_path, temp_path, overwrite_mode)
        })
        .buffer_unordered(parallelism)
        .for_each(|_| future::ready(())) // do nothing, just complete the future
        .await;

    Ok(())
}

fn make_temp_file_name(name: &OsStr) -> OsString {
    let prepend = OsStr::new("~!");
    let mut res = OsString::with_capacity(prepend.len() + name.len());
    res.push(prepend);
    res.push(name);
    res
}

fn get_credentials(credential_file: &str) -> Result<(String, String)> {
    if let Ok(mut file) = fs::File::open(credential_file) {
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|_| "Unable to read credentials")?;
        if let Ok(login) = serde_json::from_str::<Login>(&content) {
            Ok((login.username, login.password))
        } else {
            println!("Corrupt credentials.json, deleting file...");
            fs::remove_file(Path::new(credential_file))
                .map_err(|_| "Unable to delete credential file")?;
            get_credentials(credential_file)
        }
    } else {
        let username = get_input("Username (include the nusstu\\ prefix): ");
        let password = get_password("Password: ");
        Ok((username, password))
    }
}

fn store_credentials(credential_file: &str, username: &str, password: &str) -> Result<()> {
    if confirm("Store credentials (WARNING: they are stored in plain text)? [y/n]") {
        let login = Login {
            username: username.to_owned(),
            password: password.to_owned(),
        };
        let serialised =
            serde_json::to_string(&login).map_err(|_| "Unable to serialise credentials")?;
        fs::write(credential_file, serialised)
            .map_err(|_| "Unable to write to credentials file")?;
    }
    Ok(())
}

fn confirm(prompt: &str) -> bool {
    print!("{} ", prompt);
    flush_stdout();
    let mut answer = String::new();
    while answer != "y" && answer != "n" {
        answer = get_input("");
        answer.make_ascii_lowercase();
    }
    answer == "y"
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(feature = "with-env-logger")]
    env_logger::init();

    let matches = App::new(PKG_NAME)
        .version(VERSION)
        .author(&*format!("{} and contributors", clap::crate_authors!(", ")))
        .about(DESCRIPTION)
        .arg(Arg::with_name("announcements").long("announcements"))
        .arg(Arg::with_name("files").long("files"))
        .arg(
            Arg::with_name("download")
                .long("download-to")
                .takes_value(true),
        )
        .arg(Arg::with_name("list-multimedia").long("list-multimedia"))
        .arg(
            Arg::with_name("download-multimedia")
                .long("download-multimedia-to")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("credential-file")
                .long("credential-file")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("include-uploadable")
                .long("include-uploadable-folders")
                .takes_value(true)
                .min_values(0)
                .max_values(u64::max_value())
                .possible_values(&["taking", "teaching", "all"]),
        )
        .arg(
            Arg::with_name("updated")
                .long("updated")
                .takes_value(true)
                .value_name("action-on-updated-files")
                .possible_values(&["skip", "overwrite", "rename"])
                .number_of_values(1)
                .default_value("skip"),
        )
        .arg(
            Arg::with_name("term")
                .long("term")
                .takes_value(true)
                .value_name("term")
                .number_of_values(1),
        )
        .arg(
            Arg::with_name("ffmpeg")
                .long("ffmpeg")
                .takes_value(true)
                .value_name("ffmpeg-path")
                .number_of_values(1)
                .default_value("ffmpeg")
                .help("Path to ffmpeg executable for downloading multimedia"),
        )
        .get_matches();
    let credential_file = matches
        .value_of("credential-file")
        .unwrap_or("login.json")
        .to_owned();
    let do_announcements = matches.is_present("announcements");
    let do_files = matches.is_present("files");
    let download_destination = matches.value_of("download").map(|s| s.to_owned());
    let do_multimedia = matches.is_present("list-multimedia");
    let multimedia_download_destination = matches
        .value_of("download-multimedia")
        .map(|s| s.to_owned());
    let include_uploadable_folders = matches
        .values_of("include-uploadable")
        .map(|values| {
            let include_flags = values
                .fold(Ok(ModuleTypeFlags::empty()), |acc, s| {
                    acc.and_then(|flag| match s.to_lowercase().as_str() {
                        "taking" => Ok(flag | ModuleTypeFlags::TAKING),
                        "teaching" => Ok(flag | ModuleTypeFlags::TEACHING),
                        "all" => Ok(flag | ModuleTypeFlags::all()),
                        _ => Err("Invalid module type"),
                    })
                })
                .expect("Unable to parse parameters of include-uploadable");
            if include_flags.is_empty() {
                ModuleTypeFlags::all()
            } else {
                include_flags
            }
        })
        .unwrap_or_else(ModuleTypeFlags::empty);
    let overwrite_mode = matches
        .value_of("updated")
        .map(|s| match s.to_lowercase().as_str() {
            "skip" => OverwriteMode::Skip,
            "overwrite" => OverwriteMode::Overwrite,
            "rename" => OverwriteMode::Rename,
            _ => panic!("Unable to parse parameter of overwrite_mode"),
        })
        .unwrap_or(OverwriteMode::Skip);
    let specified_term = matches.value_of("term").map(|s| {
        if s.len() == 4 && s.chars().all(char::is_numeric) {
            s.to_owned()
        } else {
            panic!("Invalid input term")
        }
    });

    let (username, password) =
        get_credentials(&credential_file).expect("Unable to get credentials");

    let api = Api::with_login(&username, &password)
        .await?
        .with_ffmpeg(matches.value_of("ffmpeg").unwrap_or("ffmpeg").to_owned());
    if !Path::new(&credential_file).exists() {
        match store_credentials(&credential_file, &username, &password) {
            Ok(_) => (),
            Err(e) => println!("Failed to store credentials: {}", e),
        }
    }

    let name = api.name().await?;
    println!("Hi {}!", name);
    let modules = api.modules(specified_term).await?;
    println!("You are taking:");
    for module in modules.iter().filter(|m| m.is_taking()) {
        println!("- {} {}", module.code, module.name);
    }
    println!("You are teaching:");
    for module in modules.iter().filter(|m| m.is_teaching()) {
        println!("- {} {}", module.code, module.name);
    }

    if do_announcements {
        print_announcements(&api, &modules).await?;
    }

    if do_files || download_destination.is_some() {
        let module_file = load_modules_files(&api, &modules, include_uploadable_folders).await?;

        if do_files {
            list_resources(&module_file);
        }

        if let Some(destination) = download_destination {
            download_resources(&api, &module_file, &destination, overwrite_mode, 64).await?;
        }
    }

    if do_multimedia || multimedia_download_destination.is_some() {
        let module_multimedia = load_modules_multimedia(&api, &modules).await?;

        if do_multimedia {
            list_resources(&module_multimedia);
        }

        if let Some(destination) = multimedia_download_destination {
            download_resources(&api, &module_multimedia, &destination, overwrite_mode, 4).await?;
        }
    }

    Ok(())
}
