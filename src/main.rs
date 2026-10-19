// Copyright (c) 2026 Aiven, Helsinki, Finland. https://aiven.io/
use argh::FromArgs;
use miniserde::{Deserialize, json};
use std::fs::{File, read_link, read_to_string};
use std::io::stdout;
use std::os::fd::AsRawFd;
use std::os::linux::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tar::{EntryType, Header};

#[derive(FromArgs)]
/// Break a large file in smaller chunks.
struct Arguments {
    /// identifier of the image to extract
    #[argh(positional)]
    image_reference: String,
    /// path to podman storage dir
    #[argh(option, default = "default_storage_dir()")]
    storage_dir: PathBuf,
}

#[derive(Deserialize)]
struct Image {
    names: Option<Vec<String>>,
    layer: String,
}

fn default_storage_dir() -> PathBuf {
    std::env::home_dir()
        .map(|h| h.join(".local/share/containers/storage"))
        .unwrap_or(Path::new(".").to_path_buf())
}

fn main() {
    let args: Arguments = argh::from_env();
    let storage_dir = args.storage_dir;
    let image_reference = args.image_reference;
    let images_json_path = storage_dir.join("overlay-images/images.json");
    let images_json = read_to_string(images_json_path).expect("Missing images.json");
    let images: Vec<Image> = json::from_str(&images_json).expect("Could not parse images.json");
    for image in images {
        if image.names.unwrap_or(vec![]).contains(&image_reference) {
            let stdout_fd = stdout().as_raw_fd();
            unsafe {
                libc::fcntl(stdout_fd, 1031);
            }
            let mut builder = tar::Builder::new(stdout());
            let top_layer = image.layer;
            let top_layer_dir = storage_dir.join("overlay").join(&top_layer);
            let top_layer_diff_dir = top_layer_dir.join("diff");
            let lower_list_path = top_layer_dir.join("lower");
            let lower_list = read_to_string(lower_list_path).expect("Missing lower layers list");
            let lower_layer_dirs = lower_list.split(":").map(|l| {
                storage_dir
                    .join("overlay")
                    .join(l)
                    .canonicalize()
                    .expect("Could not resolve lower layer")
            });
            let mut files_to_skip = patricia_tree::PatriciaSet::new();
            let mut dirs_to_skip = patricia_tree::PatriciaSet::new();
            let mut next_files_to_skip = patricia_tree::PatriciaSet::new();
            let mut next_dirs_to_skip = patricia_tree::PatriciaSet::new();

            for layer_dir in [top_layer_diff_dir].into_iter().chain(lower_layer_dirs) {
                let mut entries_count = 0u64;
                let mut bytes_count = 0u64;
                let layer_start_time = Instant::now();
                walkdir::WalkDir::new(layer_dir.clone())
                    .into_iter()
                    .filter_entry(|entry| {
                        let entry_path = entry.path().strip_prefix(&layer_dir).unwrap().to_str().unwrap();
                        let metadata = entry.metadata().unwrap_or_else(|_| panic!("Could not stat: path={entry_path}"));
                        let mode = metadata.st_mode();
                        let entry_type = mode & libc::S_IFMT;
                        let is_character_device = (entry_type & libc::S_IFCHR) != 0;
                        if is_character_device && (metadata.st_rdev() == 0) {
                            next_dirs_to_skip.insert(String::from(entry_path));
                            false
                        } else if dirs_to_skip.contains(entry_path) {
                            false
                        } else if files_to_skip.contains(entry_path) {
                            true
                        } else {
                            entries_count += 1;
                            let mut header = Header::new_gnu();
                            let mode = metadata.st_mode();
                            header.set_mode(mode);
                            let uid = metadata.st_uid() as u64;
                            let gid = metadata.st_gid() as u64;
                            header.set_uid(if uid == 1001 { 0 } else { (uid % 65536) + 1 });
                            header.set_gid(if gid == 1001 { 0 } else { (gid % 65536) + 1 });
                            let is_file = (entry_type & libc::S_IFREG) != 0;
                            let is_link = (entry_type & libc::S_IFLNK) != 0;
                            let is_dir = (entry_type & libc::S_IFDIR) != 0;
                            if is_file {
                                bytes_count += metadata.st_size();
                                header.set_size(metadata.st_size());
                                header.set_entry_type(EntryType::Regular);
                                if let Ok(file) = File::open(entry.path()) {
                                    builder
                                        .append_data(&mut header, entry_path, file)
                                        .unwrap_or_else(|_| panic!("Could not append file: path={entry_path}"));
                                } else {
                                    panic!("Could not read file: uid={uid} gid={gid} mode=Oo{mode:o} path={entry_path}");
                                }
                                next_dirs_to_skip.insert(String::from(entry_path));
                                false
                            } else if is_link {
                                header.set_size(0);
                                header.set_entry_type(EntryType::Symlink);
                                let link_target =
                                    read_link(entry.path()).unwrap_or_else(|_| panic!("Could not read symlink: path={entry_path}"));
                                builder
                                    .append_link(&mut header, entry_path, link_target)
                                    .unwrap_or_else(|_| panic!("Could not append link: path={entry_path}"));
                                next_dirs_to_skip.insert(String::from(entry_path));
                                false
                            } else if is_dir {
                                if !entry_path.is_empty() { // The root dir is enumerated
                                    header.set_size(0);
                                    header.set_entry_type(EntryType::Directory);
                                    builder
                                        .append_data(&mut header, entry_path, [].as_slice())
                                        .unwrap_or_else(|_|panic!("Could not append dir: path={entry_path}"));
                                    next_files_to_skip.insert(String::from(entry_path));
                                }
                                true
                            } else {
                                false
                            }
                        }
                    })
                    .last();
                let layer_dir_str = layer_dir
                    .strip_prefix(&storage_dir)
                    .unwrap()
                    .to_str()
                    .unwrap();
                let layer_duration = layer_start_time.elapsed().as_secs_f64();
                eprintln!(
                    "Exported layer: path={layer_dir_str} entries={entries_count} bytes={bytes_count} secs={layer_duration:.1}s"
                );
                files_to_skip.extend(next_files_to_skip);
                next_files_to_skip = patricia_tree::PatriciaSet::new();
                dirs_to_skip.extend(next_dirs_to_skip);
                next_dirs_to_skip = patricia_tree::PatriciaSet::new();
            }
            return;
        }
    }
}
