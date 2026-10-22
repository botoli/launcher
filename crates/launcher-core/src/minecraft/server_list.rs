use super::launcher::VanillaLaunchConfig;
use fastnbt::Value;
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    path::Path,
};

const MAX_SERVER_LIST_BYTES: u64 = 16 * 1024 * 1024;
const PROJECT_NAME: &str = "Nevermore";

pub(super) async fn prepare_server_list(config: &VanillaLaunchConfig) -> io::Result<()> {
    let Some(address) = config
        .server_address
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    let directory = config.game_directory.clone();
    let address = address.to_owned();
    tokio::task::spawn_blocking(move || save_project_server(&directory, &address))
        .await
        .map_err(io::Error::other)?
}

// Minecraft Java stores servers.dat as an uncompressed NBT compound.
fn save_project_server(directory: &Path, address: &str) -> io::Result<()> {
    let path = directory.join("servers.dat");
    let mut root: HashMap<String, Value> = match fs::File::open(&path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(MAX_SERVER_LIST_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_SERVER_LIST_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "servers.dat is too large",
                ));
            }
            fastnbt::from_bytes(&bytes).map_err(invalid_nbt)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => HashMap::new(),
        Err(error) => return Err(error),
    };
    let servers = root
        .entry("servers".to_owned())
        .or_insert_with(|| Value::List(Vec::new()));
    let Value::List(servers) = servers else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "servers.dat servers tag is not a list",
        ));
    };
    if !servers
        .iter()
        .all(|server| matches!(server, Value::Compound(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "servers.dat contains an invalid server entry",
        ));
    }
    // Preserve custom names and fields. Update only the project entry if its address changes.
    if servers.iter().any(|server| {
        matches!(server, Value::Compound(fields)
        if fields.get("ip") == Some(&Value::String(address.to_owned())))
    }) {
        return Ok(());
    }
    if let Some(Value::Compound(fields)) = servers.iter_mut().find(|server| matches!(server,
        Value::Compound(fields) if fields.get("name") == Some(&Value::String(PROJECT_NAME.to_owned())))) {
        fields.insert("ip".to_owned(), Value::String(address.to_owned()));
    } else {
        servers.insert(0, Value::Compound(HashMap::from([
            ("name".to_owned(), Value::String(PROJECT_NAME.to_owned())),
            ("ip".to_owned(), Value::String(address.to_owned())),
        ])));
    }
    let bytes = fastnbt::to_bytes(&root).map_err(invalid_nbt)?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

fn invalid_nbt(error: fastnbt::error::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(directory: &Path) -> HashMap<String, Value> {
        fastnbt::from_bytes(&fs::read(directory.join("servers.dat")).unwrap()).unwrap()
    }

    #[test]
    fn adds_server_without_duplicates_and_updates_backend_address() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("game");
        save_project_server(&directory, "example.org:25566").unwrap();
        let first = fs::read(directory.join("servers.dat")).unwrap();
        save_project_server(&directory, "example.org:25566").unwrap();
        assert_eq!(fs::read(directory.join("servers.dat")).unwrap(), first);
        save_project_server(&directory, "[::1]:25565").unwrap();
        let root = read(&directory);
        let Value::List(servers) = &root["servers"] else {
            panic!()
        };
        assert_eq!(servers.len(), 1);
        let Value::Compound(server) = &servers[0] else {
            panic!()
        };
        assert_eq!(server["ip"], Value::String("[::1]:25565".to_owned()));
    }

    #[test]
    fn preserves_other_servers_and_unknown_nbt_fields() {
        let temp = tempfile::tempdir().unwrap();
        let original = HashMap::from([
            ("custom".to_owned(), Value::Int(42)),
            (
                "servers".to_owned(),
                Value::List(vec![Value::Compound(HashMap::from([
                    (
                        "name".to_owned(),
                        Value::String("Другой сервер 🐎".to_owned()),
                    ),
                    ("ip".to_owned(), Value::String("other.example".to_owned())),
                    ("hidden".to_owned(), Value::Byte(1)),
                ]))]),
            ),
        ]);
        fs::write(
            temp.path().join("servers.dat"),
            fastnbt::to_bytes(&original).unwrap(),
        )
        .unwrap();
        save_project_server(temp.path(), "project.example").unwrap();
        let root = read(temp.path());
        assert_eq!(root["custom"], original["custom"]);
        let Value::List(servers) = &root["servers"] else {
            panic!()
        };
        let Value::List(old) = &original["servers"] else {
            panic!()
        };
        assert_eq!(servers[1], old[0]);
        let bytes = fs::read(temp.path().join("servers.dat")).unwrap();
        save_project_server(temp.path(), "other.example").unwrap();
        assert_eq!(fs::read(temp.path().join("servers.dat")).unwrap(), bytes);
    }

    #[test]
    fn leaves_corrupt_file_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.dat");
        fs::write(&path, b"invalid nbt").unwrap();
        assert!(save_project_server(temp.path(), "example.org").is_err());
        assert_eq!(fs::read(path).unwrap(), b"invalid nbt");
    }
}
