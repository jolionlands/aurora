//! Streaming video sources and full-resolution static fallback posters.
//! mpv is external, native to the host architecture, and never decodes the
//! whole clip into Aurora's GIF frame cache.

use anyhow::{bail, Context, Result};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "mp4" | "webm"))
}

pub fn mpv_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("AURORA_MPV") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        let portable = exe.with_file_name("mpv.exe");
        if portable.is_file() {
            return portable;
        }
    }
    let installed = local_dir().join("mpv/mpv.exe");
    if installed.is_file() {
        installed
    } else {
        PathBuf::from("mpv.exe")
    }
}

pub fn local_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("aurora")
}

/// Keep child processes scoped to Aurora, including abrupt daemon exit.
pub(crate) struct OwnedProcess {
    child: Child,
    job: HANDLE,
}

impl OwnedProcess {
    pub(crate) fn spawn(mut command: Command) -> Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0);
        unsafe {
            let job = CreateJobObjectW(None, None).context("create video process job")?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Err(error) = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) {
                let _ = CloseHandle(job);
                return Err(error).context("configure video process lifetime");
            }
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = CloseHandle(job);
                    return Err(error).context("start mpv (install native mpv or set AURORA_MPV)");
                }
            };
            if let Err(error) = AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = CloseHandle(job);
                return Err(error).context("attach mpv to Aurora process lifetime");
            }
            Ok(Self { child, job })
        }
    }

    pub(crate) fn running(&mut self) -> bool {
        self.child.try_wait().is_ok_and(|status| status.is_none())
    }

    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = unsafe { CloseHandle(self.job) };
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PosterSource {
    path: PathBuf,
    hash: String,
}

static POSTER_LOCK: Mutex<()> = Mutex::new(());

/// Extract one lossless full-resolution frame; the movie itself stays intact.
/// Content and source path identify the cache so edits and aliases cannot
/// accidentally resume a different video.
pub fn poster_path(path: &Path) -> Result<PathBuf> {
    if !is_video(path) {
        return Ok(path.to_path_buf());
    }
    let _guard = POSTER_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let path = std::fs::canonicalize(path).context("resolve video source")?;
    let hash = crate::index::hash_file(&path)?;
    let mut key = blake3::Hasher::new();
    key.update(path.to_string_lossy().as_bytes());
    key.update(hash.as_bytes());
    let directory = local_dir()
        .join("video-posters")
        .join(key.finalize().to_hex().as_str());
    std::fs::create_dir_all(&directory).context("create video poster cache")?;
    let poster = directory.join("00000001.png");
    let sidecar = directory.join("source.json");
    if poster.is_file() && sidecar.is_file() {
        return Ok(poster);
    }
    let mut command = Command::new(mpv_executable());
    command.args([
        "--no-config",
        "--terminal=no",
        "--media-controls=no",
        "--load-scripts=no",
        "--autoload-files=no",
        "--idle=no",
        "--keep-open=no",
        "--audio=no",
        "--vo=image",
        "--vo-image-format=png",
        "--frames=1",
    ]);
    command.arg(format!("--vo-image-outdir={}", directory.display()));
    command.arg("--").arg(&path);
    let mut process = OwnedProcess::spawn(command)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while process.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if process.running() || !poster.is_file() {
        bail!("mpv could not extract a video poster within 10 seconds");
    }
    image::image_dimensions(&poster).context("validate video poster")?;
    std::fs::write(&sidecar, serde_json::to_vec(&PosterSource { path, hash })?)
        .context("save video poster source mapping")?;
    Ok(poster)
}

/// Windows registers the poster as its static wallpaper. Recover the original
/// clip on restart only for our own cache, and only while its content matches.
pub fn source_path(path: &Path) -> PathBuf {
    let Some(directory) = path.parent() else {
        return path.to_path_buf();
    };
    if directory.parent() != Some(local_dir().join("video-posters").as_path()) {
        return path.to_path_buf();
    }
    let result = (|| -> Result<PathBuf> {
        let source: PosterSource =
            serde_json::from_slice(&std::fs::read(directory.join("source.json"))?)?;
        if !is_video(&source.path) || crate::index::hash_file(&source.path)? != source.hash {
            bail!("video source changed or disappeared");
        }
        Ok(source.path)
    })();
    result.unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn video_extension_and_static_paths() {
        assert!(is_video(Path::new("scene.MP4")));
        assert!(is_video(Path::new("scene.webm")));
        assert!(!is_video(Path::new("scene.gif")));
        assert_eq!(
            poster_path(Path::new("still.png")).unwrap(),
            Path::new("still.png")
        );
        assert_eq!(
            source_path(Path::new("outside/00000001.png")),
            Path::new("outside/00000001.png")
        );
    }

    #[test]
    fn poster_mapping_requires_matching_source_content() {
        let cache = local_dir().join("video-posters");
        std::fs::create_dir_all(&cache).unwrap();
        let directory = tempfile::tempdir_in(cache).unwrap();
        let source = directory.path().join("clip.mp4");
        let poster = directory.path().join("00000001.png");
        std::fs::write(&source, b"original video").unwrap();
        let mapping = PosterSource {
            path: source.clone(),
            hash: crate::index::hash_file(&source).unwrap(),
        };
        std::fs::write(
            directory.path().join("source.json"),
            serde_json::to_vec(&mapping).unwrap(),
        )
        .unwrap();
        assert_eq!(source_path(&poster), source);
        std::fs::write(&source, b"different video").unwrap();
        assert_eq!(source_path(&poster), poster);
        std::fs::remove_file(&source).unwrap();
        assert_eq!(source_path(&poster), poster);
    }

    #[test]
    #[ignore = "requires native mpv and AURORA_VIDEO_TEST_SOURCE fixture"]
    fn original_video_poster_and_owned_process_lifecycle() {
        let fixture = PathBuf::from(std::env::var_os("AURORA_VIDEO_TEST_SOURCE").unwrap());
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("original.mp4");
        std::fs::copy(fixture, &source).unwrap();
        let before = crate::index::hash_file(&source).unwrap();
        let poster = poster_path(&source).unwrap();
        assert_eq!(image::image_dimensions(&poster).unwrap(), (1920, 1080));
        assert_eq!(
            source_path(&poster),
            std::fs::canonicalize(&source).unwrap()
        );
        assert_eq!(crate::index::hash_file(&source).unwrap(), before);
        let mut command = Command::new(mpv_executable());
        command.args([
            "--no-config",
            "--terminal=no",
            "--idle=yes",
            "--vo=null",
            "--audio=no",
        ]);
        let mut process = OwnedProcess::spawn(command).unwrap();
        assert!(process.running());
        use windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, process.id()).unwrap() };
        drop(process);
        assert_eq!(
            unsafe { WaitForSingleObject(handle, 1000) },
            windows::Win32::Foundation::WAIT_OBJECT_0
        );
        let _ = unsafe { CloseHandle(handle) };
    }
}
