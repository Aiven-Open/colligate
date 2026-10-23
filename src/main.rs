// Copyright (c) 2026 Aiven, Helsinki, Finland. https://aiven.io/
use argh::FromArgs;
use miniserde::{Deserialize, json};
use std::collections::HashMap;
use std::fs::{File, read_link, read_to_string};
use std::io::stdout;
use std::os::fd::AsRawFd;
use std::os::linux::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tar::{EntryType, Header};

struct IdMap {
    inner_start: u64,
    outer_start: u64,
    count: u64,
}

#[derive(FromArgs)]
/// Break a large file in smaller chunks.
struct Arguments {
    /// identifier of the image to extract
    #[argh(positional)]
    image_reference: String,
    /// path to podman storage dir
    #[argh(option, default = "default_storage_dir()")]
    storage_dir: PathBuf,
    /// comma separated list of mapped uids as `inner-start:outer-start:count`, example: `0:1001:1,1:589824:65536`
    #[argh(option, default = "String::new()")]
    map_users: String,
    /// comma separated list of mapped gids as `inner-start:outer-start:count`, example: `0:1001:1,1:589824:65536`
    #[argh(option, default = "String::new()")]
    map_groups: String,
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

fn parse_id_map(value: &str) -> Vec<IdMap> {
    value
        .split(",")
        .filter(|triple| !triple.is_empty())
        .map(|triple| {
            let parts = triple
                .split(":")
                .map(str::parse)
                .collect::<Result<Vec<u64>, _>>()
                .expect("Invalid user map");
            if parts.len() != 3 {
                panic!("Invalid user map");
            }
            IdMap {
                inner_start: parts[0],
                outer_start: parts[1],
                count: parts[2],
            }
        })
        .collect()
}

fn map_id(id: u64, id_maps: &Vec<IdMap>, id_map_cache: &mut HashMap<u64, u64>, kind: &str) -> u64 {
    match id_map_cache.get(&id) {
        None => {
            let mut mapped_id = id;
            for id_map in id_maps {
                if id >= id_map.outer_start && id < id_map.outer_start + id_map.count {
                    mapped_id = id - id_map.outer_start + id_map.inner_start;
                    break;
                }
            }
            eprintln!("Mapped {kind} {id} to {mapped_id}");
            id_map_cache.insert(id, mapped_id);
            mapped_id
        }
        Some(&mapped_id) => mapped_id,
    }
}

fn main() {
    let args: Arguments = argh::from_env();
    let storage_dir = args.storage_dir;
    let image_reference = args.image_reference;
    let images_json_path = storage_dir.join("overlay-images/images.json");
    let images_json = read_to_string(images_json_path).expect("Missing images.json");
    let images: Vec<Image> = json::from_str(&images_json).expect("Could not parse images.json");
    let uid_maps = parse_id_map(&args.map_users);
    let mut uid_map_cache: HashMap<u64, u64> = HashMap::new();
    let gid_maps = parse_id_map(&args.map_groups);
    let mut gid_map_cache: HashMap<u64, u64> = HashMap::new();
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
                        let st_mode = metadata.st_mode();
                        let type_bits = st_mode & libc::S_IFMT;
                        if (type_bits == libc::S_IFCHR) && (metadata.st_rdev() == 0) {
                            next_dirs_to_skip.insert(String::from(entry_path));
                            false
                        } else if dirs_to_skip.contains(entry_path) {
                            false
                        } else if files_to_skip.contains(entry_path) {
                            true
                        } else {
                            entries_count += 1;
                            let mut header = Header::new_gnu();
                            let mode_bits = st_mode & 0o7777;
                            header.set_mode(mode_bits);
                            let uid = metadata.st_uid() as u64;
                            let gid = metadata.st_gid() as u64;
                            header.set_uid(map_id(uid, &uid_maps, &mut uid_map_cache, "uid"));
                            header.set_gid(map_id(gid, &gid_maps, &mut gid_map_cache, "gid"));
                            if type_bits == libc::S_IFREG {
                                bytes_count += metadata.st_size();
                                header.set_size(metadata.st_size());
                                header.set_entry_type(EntryType::Regular);
                                if let Ok(file) = File::open(entry.path()) {
                                    builder
                                        .append_data(&mut header, entry_path, file)
                                        .unwrap_or_else(|_| panic!("Could not append file: path={entry_path}"));
                                } else {
                                    panic!("Could not read file: uid={uid} gid={gid} mode=Oo{mode_bits:o} path={entry_path}");
                                }
                                next_dirs_to_skip.insert(String::from(entry_path));
                                false
                            } else if type_bits == libc::S_IFLNK {
                                header.set_size(0);
                                header.set_entry_type(EntryType::Symlink);
                                let link_target =
                                    read_link(entry.path()).unwrap_or_else(|_| panic!("Could not read symlink: path={entry_path}"));
                                builder
                                    .append_link(&mut header, entry_path, link_target)
                                    .unwrap_or_else(|_| panic!("Could not append link: path={entry_path}"));
                                next_dirs_to_skip.insert(String::from(entry_path));
                                false
                            } else if type_bits == libc::S_IFDIR {
                                if !entry_path.is_empty() { // The root dir is enumerated
                                    header.set_size(0);
                                    header.set_entry_type(EntryType::Directory);
                                    builder
                                        .append_data(&mut header, entry_path, [].as_slice())
                                        .unwrap_or_else(|_| panic!("Could not append dir: path={entry_path}"));
                                    next_files_to_skip.insert(String::from(entry_path));
                                }
                                true
                            } else {
                                panic!("Unsupported entry: type=0o{type_bits:o} path={entry_path}");
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
