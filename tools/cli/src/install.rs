//! `wp-rust-cache install`: the eight steps from PHP to a verified cache.

use crate::{config_path, print_status, Args};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use wprc_core::{AttachMode, Cache, Config};

const DROPIN: &str = include_str!("../../../wordpress/object-cache.php");
const DROPIN_MARK: &str = "Plugin Name: wp-rust-cache";

struct Php {
    bin: String,
    version: String,
    minor: String,
    ext_dir: PathBuf,
    scan_dir: Option<PathBuf>,
}

fn step(n: u32, what: &str) {
    println!("[{n}/8] {what}");
}

fn ok(msg: impl AsRef<str>) {
    println!("      ok  {}", msg.as_ref());
}

fn warn(msg: impl AsRef<str>) {
    println!("      !!  {}", msg.as_ref());
}

fn exec(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/local/sbin", "/sbin"].map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn detect_php() -> Result<Php, String> {
    let bin = which("php")
        .ok_or("php not found in PATH")?
        .display()
        .to_string();
    let out = exec(
        &bin,
        &[
            "-r",
            "echo PHP_VERSION, '|', PHP_MAJOR_VERSION . '.' . PHP_MINOR_VERSION, '|', ini_get('extension_dir');",
        ],
    )?;
    let mut parts = out.trim().split('|');
    let version = parts.next().unwrap_or_default().to_string();
    let minor = parts.next().unwrap_or_default().to_string();
    let ext_dir = PathBuf::from(parts.next().unwrap_or_default());
    let scan_dir = exec(&bin, &["--ini"]).ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("Scan for additional .ini files in:"))
            .map(|d| d.trim().to_string())
            .filter(|d| d != "(none)" && !d.is_empty())
            .map(PathBuf::from)
    });
    Ok(Php {
        bin,
        version,
        minor,
        ext_dir,
        scan_dir,
    })
}

fn version_ok(minor: &str) -> bool {
    let mut it = minor.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    let (a, b) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (a, b) >= (8, 2)
}

pub fn run(args: &Args) -> Result<(), String> {
    let dry = args.flag("--dry-run");
    let force = args.flag("--force");
    let wp = PathBuf::from(
        args.value("--wp")
            .ok_or("--wp PATH (the WordPress root directory) is required")?,
    );
    let user = args.value("--user").unwrap_or("www-data").to_string();
    let cfg_path = config_path(args);
    if dry {
        println!("(dry run: nothing is changed)\n");
    }

    // 1. PHP
    step(1, "PHP");
    let php = detect_php()?;
    if !version_ok(&php.minor) {
        return Err(format!(
            "PHP {} found; 8.2 or later is required",
            php.version
        ));
    }
    ok(format!("{} ({})", php.version, php.bin));

    // 2. PHP-FPM
    step(2, "PHP-FPM");
    let fpm = [format!("php-fpm{}", php.minor), "php-fpm".to_string()]
        .iter()
        .find_map(|n| which(n));
    match &fpm {
        Some(p) => ok(p.display().to_string()),
        None => warn("php-fpm not found: only the CLI will use the cache"),
    }

    // Every check that can fail runs before anything is changed.
    let content = wp.join("wp-content");
    if !wp.join("wp-config.php").is_file()
        && !wp
            .parent()
            .is_some_and(|p| p.join("wp-config.php").is_file())
    {
        return Err(format!(
            "{} does not look like a WordPress root (no wp-config.php)",
            wp.display()
        ));
    }
    let dropin = content.join("object-cache.php");
    if let Ok(existing) = fs::read_to_string(&dropin) {
        if existing != DROPIN && !existing.contains(DROPIN_MARK) && !force {
            return Err(format!(
                "{} belongs to another object cache; remove it or pass --force (it will be backed up)",
                dropin.display()
            ));
        }
    }
    let new_config = if cfg_path.exists() {
        None
    } else {
        // Size the cache to what /dev/shm can hold: half of its free space
        // (a configuration change briefly needs room for two segments),
        // at most 1 GB.
        let free = shm_available(&Config::default().path());
        let memory = (free / 2).min(1 << 30) & !((64 << 20) - 1);
        if memory < 64 << 20 {
            return Err(format!(
                "/dev/shm has {} free; at least 128 MB is needed",
                wprc_core::config::format_size(free)
            ));
        }
        Some(format!(
            "[cache]\nenabled = true\nmemory = \"{}MB\"\nshards = 64\neviction = \"tinylfu\"\n\n\
             [shared_memory]\npath = \"/dev/shm/wp-rust-cache\"\npermissions = \"0600\"\nowner = \"{user}\"\n",
            memory >> 20
        ))
    };
    let mut cfg = match &new_config {
        Some(text) => Config::parse(text)?,
        None => Config::load(&cfg_path)?,
    };
    if cfg.owner.is_none() {
        cfg.owner = Some(user.clone());
    }
    let free = shm_available(&cfg.path());
    // + a few MB for the header, name directory and generation table.
    if cfg.memory + (4 << 20) > free {
        return Err(format!(
            "memory = {} in {} but {} holds only {} more; lower it",
            wprc_core::config::format_size(cfg.memory),
            cfg_path.display(),
            cfg.path()
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            wprc_core::config::format_size(free)
        ));
    }

    // 3. extension
    step(3, "PHP extension");
    let src = match args.value("--extension") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(format!(
            "/usr/lib/wp-rust-cache/php-{}/wp_rust_cache.so",
            php.minor
        )),
    };
    if !src.is_file() {
        return Err(format!(
            "{} not found; build it with php-extension/build.sh and pass --extension FILE",
            src.display()
        ));
    }
    // A module built for another PHP version refuses to load: find out now,
    // before anything is changed.
    let probe = exec(
        &php.bin,
        &[
            "-n",
            "-d",
            &format!("extension={}", src.display()),
            "-r",
            "echo extension_loaded('wp_rust_cache') ? 'loaded' : 'not loaded';",
        ],
    )
    .unwrap_or_else(|e| e);
    if !probe.contains("loaded") || probe.contains("not loaded") || probe.contains("Warning") {
        return Err(format!(
            "{} cannot be loaded by PHP {}: {}",
            src.display(),
            php.version,
            probe.trim()
        ));
    }
    // A module shipped by the package is referenced where the package keeps
    // it, so a package upgrade reaches PHP with a reload. Anything else is
    // copied into PHP's extension directory.
    let packaged = src.starts_with("/usr/lib/wp-rust-cache");
    let dst = if packaged {
        src.clone()
    } else {
        php.ext_dir.join("wp_rust_cache.so")
    };
    let ini = format!(
        "; wp-rust-cache\nextension={}\nwp_rust_cache.config={}\n",
        if packaged {
            dst.display().to_string()
        } else {
            "wp_rust_cache.so".into()
        },
        cfg_path.display()
    );
    let phpenmod = which("phpenmod");
    let ini_path = match (&phpenmod, &php.scan_dir) {
        (Some(_), _) => PathBuf::from(format!(
            "/etc/php/{}/mods-available/wp_rust_cache.ini",
            php.minor
        )),
        (None, Some(d)) => d.join("50-wp_rust_cache.ini"),
        (None, None) => return Err("cannot find where PHP reads additional .ini files".into()),
    };
    if dry {
        if !packaged {
            ok(format!("would copy {} → {}", src.display(), dst.display()));
        }
        ok(format!("would write {}", ini_path.display()));
    } else {
        if !packaged {
            fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        }
        fs::write(&ini_path, ini).map_err(|e| format!("{}: {e}", ini_path.display()))?;
        if phpenmod.is_some() {
            exec("phpenmod", &["-v", &php.minor, "wp_rust_cache"])?;
        }
        let modules = exec(&php.bin, &["-m"])?;
        if !modules.lines().any(|l| l.trim() == "wp_rust_cache") {
            return Err("the extension was installed but PHP does not load it (php -m)".into());
        }
        ok(format!(
            "{} enabled via {}",
            dst.display(),
            ini_path.display()
        ));
    }

    // Configuration (created once, never overwritten).
    if let Some(text) = &new_config {
        if dry {
            ok(format!(
                "would write {} (memory = {})",
                cfg_path.display(),
                wprc_core::config::format_size(cfg.memory)
            ));
        } else {
            if let Some(dir) = cfg_path.parent() {
                fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            fs::write(&cfg_path, text).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
            ok(format!(
                "wrote {} (memory = {})",
                cfg_path.display(),
                wprc_core::config::format_size(cfg.memory)
            ));
        }
    }

    // 4. drop-in
    step(4, "object-cache.php");
    if dry {
        ok(format!("would install {}", dropin.display()));
    } else {
        if dropin.exists() && force {
            let bak = content.join(format!("object-cache.php.bak-{}", wprc_core::now_secs()));
            fs::rename(&dropin, &bak).map_err(|e| e.to_string())?;
            ok(format!("previous drop-in saved as {}", bak.display()));
        }
        fs::write(&dropin, DROPIN).map_err(|e| format!("{}: {e}", dropin.display()))?;
        if let Ok(m) = fs::metadata(&content) {
            let _ = std::os::unix::fs::chown(&dropin, Some(m.uid()), Some(m.gid()));
        }
        ok(dropin.display().to_string());
    }

    // 5. shared memory
    step(5, "shared memory");
    let path = cfg.path();
    if dry {
        ok(format!(
            "would create {} ({})",
            path.display(),
            wprc_core::config::format_size(cfg.memory)
        ));
    } else {
        let cache = Cache::attach(&cfg, AttachMode::Create).map_err(|e| e.to_string())?;
        ok(format!(
            "{} ({})",
            path.display(),
            wprc_core::config::format_size(cache.stats().total_size)
        ));
    }

    // 6. permissions
    step(6, "permissions");
    if !dry {
        let m = fs::metadata(&path).map_err(|e| e.to_string())?;
        let mode = m.permissions().mode() & 0o777;
        if mode & 0o007 != 0 {
            return Err(format!(
                "{} is accessible to other users (mode {mode:o})",
                path.display()
            ));
        }
        let want = user_uid(&user);
        match want {
            Some(uid) if uid != m.uid() => {
                return Err(format!(
                    "{} is owned by uid {}, not {user}",
                    path.display(),
                    m.uid()
                ))
            }
            _ => ok(format!("mode {mode:o}, owner {user}")),
        }
    }

    // 7. self-test through the real extension
    step(7, "self-test");
    if !dry {
        let code = "$g = wp_rust_cache_group('wp-rust-cache-selftest', 'selftest');\
            $ok = $g !== false\
              && wp_rust_cache_set($g, 0, 'k', ['v' => 1])\
              && wp_rust_cache_get($g, 0, 'k') === ['v' => 1]\
              && wp_rust_cache_set($g, 0, 'n', 1)\
              && wp_rust_cache_incr($g, 0, 'n', 2) === 3\
              && wp_rust_cache_delete($g, 0, 'k')\
              && wp_rust_cache_get($g, 0, 'k') === false;\
            wp_rust_cache_flush_namespace('wp-rust-cache-selftest');\
            echo $ok ? 'OK' : 'FAIL ' . json_encode(wp_rust_cache_info());";
        let cfg_arg = format!("wp_rust_cache.config={}", cfg_path.display());
        let out = exec(&php.bin, &["-d", &cfg_arg, "-r", code])?;
        if out.trim() != "OK" {
            return Err(format!("self-test failed: {}", out.trim()));
        }
        ok("set, get, incr, delete through the extension");
    }

    // 8. status
    step(8, "status");
    if !dry {
        let cache = Cache::attach(&cfg, AttachMode::Existing).map_err(|e| e.to_string())?;
        println!();
        print_status(&cache.stats());
    }
    if let Some(p) = fpm {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!("\nReload PHP-FPM so its workers load the extension:\n  systemctl reload {name}");
    }
    Ok(())
}

/// `wp-rust-cache uninstall`: the reverse of `install`, in the safe order —
/// first WordPress stops calling the cache, then PHP stops loading the
/// extension, then the memory is released. The configuration file stays.
pub fn uninstall(args: &Args) -> Result<(), String> {
    let dry = args.flag("--dry-run");
    let wp = PathBuf::from(
        args.value("--wp")
            .ok_or("--wp PATH (the WordPress root directory) is required")?,
    );
    let cfg_path = config_path(args);
    if dry {
        println!("(dry run: nothing is changed)\n");
    }
    let act = |what: String, f: &dyn Fn() -> Result<(), String>| -> Result<(), String> {
        if dry {
            ok(format!("would {what}"));
            Ok(())
        } else {
            f()?;
            ok(what);
            Ok(())
        }
    };

    println!("[1/3] object-cache.php");
    let dropin = wp.join("wp-content").join("object-cache.php");
    match fs::read_to_string(&dropin) {
        Ok(text) if text.contains(DROPIN_MARK) => {
            act(format!("remove {}", dropin.display()), &|| {
                fs::remove_file(&dropin).map_err(|e| format!("{}: {e}", dropin.display()))
            })?
        }
        Ok(_) => warn(format!(
            "{} belongs to another cache; left alone",
            dropin.display()
        )),
        Err(_) => ok("no drop-in installed"),
    }

    println!("[2/3] PHP extension");
    let php = detect_php()?;
    let so = php.ext_dir.join("wp_rust_cache.so");
    if which("phpdismod").is_some() {
        act(format!("phpdismod -v {} wp_rust_cache", php.minor), &|| {
            exec("phpdismod", &["-v", &php.minor, "wp_rust_cache"]).map(|_| ())
        })?;
        let avail = PathBuf::from(format!(
            "/etc/php/{}/mods-available/wp_rust_cache.ini",
            php.minor
        ));
        if avail.exists() {
            act(format!("remove {}", avail.display()), &|| {
                fs::remove_file(&avail).map_err(|e| e.to_string())
            })?;
        }
    } else if let Some(ini) = php
        .scan_dir
        .as_ref()
        .map(|d| d.join("50-wp_rust_cache.ini"))
    {
        if ini.exists() {
            act(format!("remove {}", ini.display()), &|| {
                fs::remove_file(&ini).map_err(|e| format!("{}: {e}", ini.display()))
            })?;
        }
    }
    if so.exists() {
        act(format!("remove {}", so.display()), &|| {
            fs::remove_file(&so).map_err(|e| format!("{}: {e}", so.display()))
        })?;
    }

    println!("[3/3] shared memory");
    let cfg = Config::load(&cfg_path)?;
    let path = cfg.path();
    if path.exists() {
        act(
            format!("retire and remove {}", path.display()),
            &|| match Cache::attach(&cfg, AttachMode::Existing) {
                Ok(c) => c.retire().map_err(|e| e.to_string()),
                Err(_) => fs::remove_file(&path).map_err(|e| e.to_string()),
            },
        )?;
    } else {
        ok("no segment");
    }
    println!(
        "\n{} is kept. Reload PHP-FPM so its workers drop the extension:\n  systemctl reload php{}-fpm",
        cfg_path.display(),
        php.minor
    );
    Ok(())
}

/// Bytes the segment at `path` can use: free space in its filesystem plus
/// the size of a segment already there (it is reused or replaced).
fn shm_available(path: &std::path::Path) -> u64 {
    let dir = path.parent().unwrap_or(std::path::Path::new("/dev/shm"));
    let existing = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return existing;
    };
    // SAFETY: `st` is a valid out-pointer and `c` a NUL-terminated path.
    let free = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return existing;
        }
        st.f_bavail as u64 * st.f_frsize as u64
    };
    free + existing
}

fn user_uid(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam returns NULL or static storage; we read one integer.
    unsafe {
        let p = libc::getpwnam(c.as_ptr());
        (!p.is_null()).then(|| (*p).pw_uid)
    }
}
