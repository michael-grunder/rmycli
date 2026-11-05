//! Version 1 credential envelope. See README for the byte-level format.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;

use alkali::{
    hash::pbkdf::argon2id, symmetric::aead::xchacha20poly1305_ietf as aead,
};
use rustix::fs::{self, AtFlags, FlockOperation, Mode, OFlags};
use zeroize::Zeroizing;

use crate::auth::AuthFile;

const MAGIC: &[u8; 8] = b"RMYAUTH\0";
const HEADER_LEN: usize = 60;
const MAX_FILE: usize = 8 * 1024 * 1024;
const FILE: &str = "auth.enc";
const LOCK: &str = "auth.lock";
const MEMORY: u32 = 64 * 1024 * 1024;
const TIME: u32 = 2;

struct Header {
    bytes: [u8; HEADER_LEN],
}

impl Header {
    fn new() -> io::Result<Self> {
        let mut bytes = [0; HEADER_LEN];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..10].copy_from_slice(&1_u16.to_be_bytes());
        bytes[10] = 1; // Argon2id 1.3, one lane (libsodium).
        bytes[11] = 1; // XChaCha20-Poly1305-IETF.
        bytes[12..16].copy_from_slice(&MEMORY.to_be_bytes());
        bytes[16..20].copy_from_slice(&TIME.to_be_bytes());
        bytes[20..36]
            .copy_from_slice(&argon2id::generate_salt().map_err(crypto_error)?);
        Ok(Self { bytes })
    }

    fn parse(bytes: &[u8]) -> io::Result<Self> {
        if !(HEADER_LEN + aead::MAC_LENGTH..=MAX_FILE).contains(&bytes.len()) {
            return Err(invalid("invalid encrypted auth file size"));
        }
        let header = Self {
            bytes: bytes[..HEADER_LEN].try_into().expect("checked header size"),
        };
        if &header.bytes[..8] != MAGIC
            || header.bytes[8..10] != 1_u16.to_be_bytes()
            || header.bytes[10..12] != [1, 1]
        {
            return Err(invalid(
                "unsupported encrypted auth format or algorithm",
            ));
        }
        if !(MEMORY..=256 * 1024 * 1024).contains(&header.memory())
            || !(TIME..=6).contains(&header.time())
        {
            return Err(invalid(
                "encrypted auth KDF costs exceed supported bounds",
            ));
        }
        Ok(header)
    }

    fn memory(&self) -> u32 {
        u32::from_be_bytes(self.bytes[12..16].try_into().unwrap())
    }

    fn time(&self) -> u32 {
        u32::from_be_bytes(self.bytes[16..20].try_into().unwrap())
    }

    fn key(
        &self,
        passphrase: &[u8],
    ) -> io::Result<aead::Key<alkali::mem::FullAccess>> {
        let mut key = aead::Key::new_empty().map_err(crypto_error)?;
        argon2id::derive_key(
            passphrase,
            self.bytes[20..36].try_into().unwrap(),
            self.time() as usize,
            self.memory() as usize,
            key.as_mut(),
        )
        .map_err(crypto_error)?;
        Ok(key)
    }
}

struct Unlocked {
    header: Header,
    key: aead::Key<alkali::mem::FullAccess>,
    file: AuthFile,
    snapshot: Option<Vec<u8>>,
}

/// Shared by connection clones; locking drops the only cached key/payload.
#[derive(Default)]
pub struct Vault {
    unlocked: Option<Unlocked>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("unlocked", &self.unlocked.is_some())
            .finish()
    }
}

impl Vault {
    pub const fn is_unlocked(&self) -> bool {
        self.unlocked.is_some()
    }

    pub fn lock(&mut self) {
        self.unlocked = None;
    }

    pub fn unlock(
        &mut self,
        path: &Path,
        mut prompt: impl FnMut(bool) -> io::Result<Zeroizing<String>>,
    ) -> io::Result<()> {
        if self.is_unlocked() {
            return Ok(());
        }
        let snapshot = read_path(path)?;
        // Validate attacker-controlled costs before prompting or allocating KDF memory.
        let header = snapshot
            .as_ref()
            .map_or_else(Header::new, |bytes| Header::parse(bytes))?;
        let passphrase = prompt(snapshot.is_none())?;
        let key = header.key(passphrase.as_bytes())?;
        drop(passphrase);
        let file = if let Some(bytes) = &snapshot {
            let mut plaintext =
                Zeroizing::new(vec![
                    0;
                    bytes.len() - HEADER_LEN - aead::MAC_LENGTH
                ]);
            aead::decrypt(
                &bytes[HEADER_LEN..],
                Some(&header.bytes),
                &key,
                header.bytes[36..60].try_into().unwrap(),
                &mut plaintext,
            )
            .map_err(|_| invalid("wrong passphrase or damaged file"))?;
            let file: AuthFile = serde_json::from_slice(&plaintext)
                .map_err(|_| invalid("invalid encrypted credential payload"))?;
            file.validate()?;
            file
        } else {
            AuthFile::default()
        };
        self.unlocked = Some(Unlocked {
            header,
            key,
            file,
            snapshot,
        });
        Ok(())
    }

    pub fn import(&mut self, path: &Path, file: AuthFile) -> io::Result<()> {
        if self.state()?.snapshot.is_some() {
            return Err(io::Error::other(
                "encrypted auth file already exists; refusing to overwrite it",
            ));
        }
        self.update(path, |target| {
            *target = file;
            Ok(())
        })
    }

    pub fn file(&self) -> io::Result<&AuthFile> {
        Ok(&self.state()?.file)
    }

    fn state(&self) -> io::Result<&Unlocked> {
        self.unlocked.as_ref().ok_or_else(|| {
            io::Error::other("credential store is locked; use \\auth unlock")
        })
    }

    pub fn update(
        &mut self,
        path: &Path,
        edit: impl FnOnce(&mut AuthFile) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut file = self.state()?.file.clone();
        edit(&mut file)?;
        let state = self.unlocked.as_mut().unwrap();
        if let Err(error) = persist(path, state, &file) {
            // A rename may have succeeded even if fsync failed; reload next time.
            self.lock();
            return Err(error);
        }
        state.file = file;
        Ok(())
    }

    pub fn change_passphrase(
        &mut self,
        path: &Path,
        passphrase: &[u8],
    ) -> io::Result<()> {
        let state = self.state()?;
        let header = Header::new()?;
        let key = header.key(passphrase)?;
        let mut replacement = Unlocked {
            header,
            key,
            file: state.file.clone(),
            snapshot: state.snapshot.clone(),
        };
        let file = std::mem::take(&mut replacement.file);
        if let Err(error) = persist(path, &mut replacement, &file) {
            self.lock();
            return Err(error);
        }
        replacement.file = file;
        self.unlocked = Some(replacement);
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn crypto_error(_: alkali::AlkaliError) -> io::Error {
    io::Error::other("credential cryptography failed")
}

fn directory(path: &Path, create: bool) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("auth file needs a parent directory"))?;
    if create {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let dir = File::from(fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let metadata = dir.metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(invalid(
            "encrypted auth directory must be owned by you with mode 0700",
        ));
    }
    Ok(dir)
}

fn open(dir: &File, name: &str, flags: OFlags) -> io::Result<File> {
    let file = File::from(fs::openat(
        dir,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )?);
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(invalid(
            "encrypted auth files must be regular, single-link, owned by you, and mode 0600",
        ));
    }
    Ok(file)
}

fn read(dir: &File) -> io::Result<Option<Vec<u8>>> {
    let file = match open(dir, FILE, OFlags::RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_FILE as u64 {
        return Err(invalid("encrypted auth file is too large"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE as u64 + 1).read_to_end(&mut bytes)?;
    Header::parse(&bytes)?;
    Ok(Some(bytes))
}

fn read_path(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match directory(path, false) {
        Ok(dir) => read(&dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn persist(
    path: &Path,
    state: &mut Unlocked,
    file: &AuthFile,
) -> io::Result<()> {
    let dir = directory(path, true)?;
    let lock = open(&dir, LOCK, OFlags::RDWR | OFlags::CREATE)?;
    fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(
        |_| io::Error::other("encrypted auth file is busy; retry later"),
    )?;
    if read(&dir)? != state.snapshot {
        return Err(io::Error::other(
            "encrypted auth file changed since unlocking; lock and unlock again before retrying",
        ));
    }
    let mut plaintext =
        Zeroizing::new(vec![0; MAX_FILE - HEADER_LEN - aead::MAC_LENGTH]);
    let mut cursor = io::Cursor::new(plaintext.as_mut_slice());
    serde_json::to_writer(&mut cursor, file).map_err(|_| {
        invalid("cannot serialize credentials within the file size limit")
    })?;
    let length = usize::try_from(cursor.position()).expect("bounded payload");
    plaintext.truncate(length);
    let nonce = aead::generate_nonce().map_err(crypto_error)?;
    state.header.bytes[36..60].copy_from_slice(&nonce);
    let mut bytes = vec![0; HEADER_LEN + plaintext.len() + aead::MAC_LENGTH];
    bytes[..HEADER_LEN].copy_from_slice(&state.header.bytes);
    aead::encrypt(
        &plaintext,
        Some(&state.header.bytes),
        &state.key,
        Some(&nonce),
        &mut bytes[HEADER_LEN..],
    )
    .map_err(crypto_error)?;
    drop(plaintext);
    let name = format!(
        ".auth-{:032x}.tmp",
        u128::from_be_bytes(argon2id::generate_salt().map_err(crypto_error)?)
    );
    let mut temporary = File::from(fs::openat(
        &dir,
        &name,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    let result = (|| {
        fs::fchmod(&temporary, Mode::RUSR | Mode::WUSR)?;
        temporary.write_all(&bytes)?;
        temporary.sync_all()?;
        fs::renameat(&dir, &name, &dir, FILE)?;
        // Record a successful rename even if the durability check fails.
        state.snapshot = Some(bytes);
        dir.sync_all()
    })();
    let _ = fs::unlinkat(&dir, &name, AtFlags::empty());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn unlock(
        vault: &mut Vault,
        path: &Path,
        password: &str,
    ) -> io::Result<()> {
        vault.unlock(path, |_| Ok(Zeroizing::new(password.into())))
    }

    fn credentials() -> AuthFile {
        toml::from_str("[servers.'redis://localhost:6379']\npassword = 'redis-secret'\nusers = { alice = 'alice-secret' }\ndefault-user = 'alice'\n").unwrap()
    }

    fn create(path: &Path) -> Vault {
        let mut vault = Vault::default();
        unlock(&mut vault, path, "master-secret").unwrap();
        vault.import(path, credentials()).unwrap();
        vault
    }

    #[test]
    fn round_trip_lock_rotation_and_portability() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/auth.enc");
        let mut vault = create(&path);
        let first = std::fs::read(&path).unwrap();
        for secret in [
            b"redis-secret".as_slice(),
            b"alice-secret",
            b"master-secret",
            b"localhost",
        ] {
            assert!(!first.windows(secret.len()).any(|part| part == secret));
        }
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        for name in [FILE, LOCK] {
            assert_eq!(
                std::fs::metadata(path.with_file_name(name)).unwrap().mode()
                    & 0o777,
                0o600
            );
        }
        assert!(std::fs::read_dir(path.parent().unwrap()).unwrap().all(
            |entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            }
        ));
        vault.update(&path, |_| Ok(())).unwrap();
        let second = std::fs::read(&path).unwrap();
        assert_eq!(&first[..36], &second[..36]);
        assert_ne!(&first[36..60], &second[36..60]);
        vault.lock();
        assert!(vault.file().is_err());
        assert_eq!(
            unlock(&mut vault, &path, "wrong").unwrap_err().to_string(),
            "wrong passphrase or damaged file"
        );
        unlock(&mut vault, &path, "master-secret").unwrap();
        assert_eq!(
            serde_json::to_value(vault.file().unwrap()).unwrap(),
            serde_json::to_value(credentials()).unwrap()
        );
        vault.change_passphrase(&path, b"new-master").unwrap();
        let rotated = std::fs::read(&path).unwrap();
        assert_ne!(&second[20..36], &rotated[20..36]);
        assert_ne!(&second[36..60], &rotated[36..60]);
        vault.lock();
        assert!(unlock(&mut vault, &path, "master-secret").is_err());
        unlock(&mut vault, &path, "new-master").unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            other.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let copied = other.path().join(FILE);
        std::fs::copy(path, &copied).unwrap();
        let mut imported = Vault::default();
        unlock(&mut imported, &copied, "new-master").unwrap();
        assert_eq!(
            serde_json::to_value(imported.file().unwrap()).unwrap(),
            serde_json::to_value(credentials()).unwrap()
        );
    }

    #[test]
    fn header_bounds_are_checked_before_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/auth.enc");
        create(&path);
        let original = std::fs::read(&path).unwrap();
        for index in [0, 8, 9, 10, 11, 12, 16] {
            let mut bytes = original.clone();
            bytes[index] = 255;
            std::fs::write(&path, bytes).unwrap();
            assert!(
                Vault::default()
                    .unlock(&path, |_| panic!("invalid header prompted"))
                    .is_err()
            );
        }
        for len in 0..HEADER_LEN + aead::MAC_LENGTH {
            assert!(Header::parse(&original[..len]).is_err());
        }
        for (offset, value) in [
            (12, MEMORY - 1),
            (16, TIME - 1),
            (12, 256 * 1024 * 1024 + 1),
            (16, 7),
        ] {
            let mut bytes = original.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
            assert!(Header::parse(&bytes).is_err());
        }
        let file = File::create(&path).unwrap();
        file.set_len(MAX_FILE as u64 + 1).unwrap();
        assert!(
            Vault::default()
                .unlock(&path, |_| panic!("oversize file prompted"))
                .is_err()
        );
    }

    #[test]
    fn every_header_field_ciphertext_and_tag_are_authenticated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/auth.enc");
        create(&path);
        let original = std::fs::read(&path).unwrap();
        for index in [15, 19, 20, 35, 36, 59, 60, original.len() - 1] {
            let mut bytes = original.clone();
            bytes[index] ^= 1;
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(
                unlock(&mut Vault::default(), &path, "master-secret")
                    .unwrap_err()
                    .to_string(),
                "wrong passphrase or damaged file"
            );
        }
    }

    #[test]
    fn stale_updates_rotation_and_creation_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/auth.enc");
        let mut first = create(&path);
        let mut second = Vault::default();
        unlock(&mut second, &path, "master-secret").unwrap();
        first.update(&path, |_| Ok(())).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            second
                .update(&path, |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("changed since unlocking")
        );
        assert!(second.change_passphrase(&path, b"replacement").is_err());
        assert!(second.import(&path, AuthFile::default()).is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
        second.lock();
        unlock(&mut second, &path, "master-secret").unwrap();
        second.update(&path, |_| Ok(())).unwrap();

        let new_path = dir.path().join("new/auth.enc");
        let mut racing = Vault::default();
        unlock(&mut racing, &new_path, "master-secret").unwrap();
        create(&new_path);
        let before = std::fs::read(&new_path).unwrap();
        assert!(racing.import(&new_path, AuthFile::default()).is_err());
        assert_eq!(before, std::fs::read(&new_path).unwrap());
    }

    #[test]
    fn stable_locks_symlinks_permissions_and_failed_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/auth.enc");
        let mut vault = create(&path);
        let before = std::fs::read(&path).unwrap();
        let lock_path = path.with_file_name(LOCK);
        let inode = std::fs::metadata(&lock_path).unwrap().ino();
        let lock = File::open(&lock_path).unwrap();
        fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        assert!(
            vault
                .update(&path, |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        drop(lock);
        unlock(&mut vault, &path, "master-secret").unwrap();
        vault.update(&path, |_| Ok(())).unwrap();
        assert_eq!(inode, std::fs::metadata(&lock_path).unwrap().ino());
        assert!(
            vault
                .update(&path, |file| {
                    *file = AuthFile::default();
                    Err(io::Error::other("cancelled"))
                })
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(vault.file().unwrap()).unwrap(),
            serde_json::to_value(credentials()).unwrap()
        );
        std::fs::remove_file(&lock_path).unwrap();
        symlink(&path, &lock_path).unwrap();
        assert!(vault.update(&path, |_| Ok(())).is_err());
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let target = dir.path().join("untouched");
        std::fs::write(&target, &before).unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_path(&path).is_err());
        assert!(vault.update(&path, |_| Ok(())).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), before);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, &before).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(read_path(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .unwrap();
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(read_path(&path).is_err());
    }
}
