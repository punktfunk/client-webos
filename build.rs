fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");

    // Only webOS cross target needs glibc shim; dev box's glibc has getauxval/gettid/sendmmsg.
    if std::env::var("TARGET").as_deref() != Ok("armv7-unknown-linux-gnueabi") {
        return;
    }

    // Generate third-party notices (shown on About screen).
    generate_third_party_notices(&manifest_dir);
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=assets");
    let cc = std::env::var("CC_armv7_unknown_linux_gnueabi")
        .or_else(|_| std::env::var("CC"))
        .unwrap_or_else(|_| "cc".into());

    let obj = format!("{out_dir}/glibc_compat_shim.o");
    let status = std::process::Command::new(&cc)
        // -fPIC: the final binary links -pie (position-independent executable).
        .args(["-fPIC", "-c", "src/platform/webos/glibc_compat_shim.c", "-o"])
        .arg(&obj)
        .status()
        .unwrap_or_else(|e| panic!("run {cc} to compile glibc_compat_shim.c: {e}"));
    assert!(status.success(), "{cc} failed compiling glibc_compat_shim.c");

    // Object must come AFTER libstd in link order (so linker pulls glibc shim symbols).
    println!("cargo:rustc-link-arg={obj}");
    println!("cargo:rerun-if-changed=src/platform/webos/glibc_compat_shim.c");

    // On-device libSDL2 is too old; bundle newer version in ipk/lib/ and use $ORIGIN-relative rpath.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
}

fn generate_third_party_notices(manifest_dir: &str) {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    const BUNDLED_COMPONENTS: &[(&str, &str, Option<&str>, &str)] = &[
        ("SDL2 (webosbrew/SDL-webOS backport)",
         "Bundled as lib/libSDL2-2.0.so.0 in the .ipk (the on-device system copy is too old — see docs/NOTES.md). Zlib license.",
         None,
         "https://github.com/webosbrew/SDL-webOS"),
        ("FreeType",
         "Bundled as lib/libfreetype.so.6 in the .ipk (Skia calls entry points the on-device copy lacks below webOS 9 — see taskfiles/toolchain.yml). FreeType License (BSD-style with credit clause).",
         None,
         "https://freetype.org"),
        ("GNU libstdc++",
         "Bundled as lib/libstdc++.so.6 in the .ipk (the SDK builds against a newer GLIBCXX than the TV ships). GPL-3.0 with the GCC Runtime Library Exception.",
         None,
         "https://gcc.gnu.org/onlinedocs/libstdc++/"),
        ("Geist (Geist Sans)",
         "punktfunk's brand typeface, embedded into the binary by the console kit (pf-console-ui). SIL Open Font License 1.1.",
         Some("assets/fonts/Geist-OFL.txt"),
         "https://github.com/vercel/geist-font"),
        ("Lucide icons",
         "Icon path data embedded by the console kit (pf-console-ui). ISC License.",
         Some("assets/icons/LICENSE-lucide.txt"),
         "https://lucide.dev"),
        ("Font Awesome brand card icons",
         "Launcher and OS marks as vector tables in the console kit. Creative Commons Attribution 4.0.",
         Some("assets/cards/LICENSES/font-awesome-brands.txt"),
         "https://fontawesome.com"),
        ("Simple Icons card icons",
         "Launcher and OS marks as vector tables in the console kit. CC0 1.0 Universal.",
         Some("assets/cards/LICENSES/simple-icons.txt"),
         "https://simpleicons.org"),
        ("Bazzite card icon",
         "Bazzite mark as a vector table in the console kit. Apache License 2.0.",
         Some("assets/cards/LICENSES/bazzite.txt"),
         "https://github.com/ublue-os/bazzite"),
        ("Playnite card icon",
         "Playnite mark as a vector table in the console kit. MIT License.",
         Some("assets/cards/LICENSES/playnite.txt"),
         "https://github.com/JosefNemec/Playnite"),
        ("NDL DirectMedia",
         "LG webOS system library, `dlopen`'d at runtime from the device — NOT redistributed by this package. Header signatures were taken from mariotaku/ss4s.",
         None,
         "https://github.com/mariotaku/ss4s"),
    ];

    // Run cargo metadata to get the dependency graph filtered for the webOS target
    let metadata_output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            "armv7-unknown-linux-gnueabi",
        ])
        .current_dir(manifest_dir)
        .output()
        .expect("failed to run cargo metadata");

    if !metadata_output.status.success() {
        eprintln!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&metadata_output.stderr)
        );
        std::process::exit(1);
    }

    let metadata: serde_json::Value =
        serde_json::from_slice(&metadata_output.stdout).expect("failed to parse cargo metadata JSON");

    let ws_members: std::collections::HashSet<_> = metadata["workspace_members"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut pkgs: Vec<_> = metadata["packages"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|pkg| {
            let id = pkg["id"].as_str()?;
            if ws_members.contains(id) {
                None
            } else {
                Some(pkg.clone())
            }
        })
        .collect();

    // Sort by name and version
    pkgs.sort_by(|a, b| {
        let a_name = a["name"].as_str().unwrap_or("").to_lowercase();
        let b_name = b["name"].as_str().unwrap_or("").to_lowercase();
        match a_name.cmp(&b_name) {
            std::cmp::Ordering::Equal => {
                let a_ver = a["version"].as_str().unwrap_or("");
                let b_ver = b["version"].as_str().unwrap_or("");
                a_ver.cmp(b_ver)
            }
            other => other,
        }
    });

    // Group license texts: text -> (filename, crates)
    let mut texts: HashMap<String, (String, std::collections::BTreeSet<String>)> = HashMap::new();
    let mut no_text = Vec::new();

    for pkg in &pkgs {
        let pkg_dir = Path::new(pkg["manifest_path"].as_str().unwrap_or(""))
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let files = find_license_files(pkg_dir);
        let label = format!(
            "{} {}",
            pkg["name"].as_str().unwrap_or(""),
            pkg["version"].as_str().unwrap_or("")
        );

        if files.is_empty() {
            no_text.push(pkg.clone());
            continue;
        }

        for (fname, txt) in files {
            texts
                .entry(txt)
                .or_insert_with(|| (fname, std::collections::BTreeSet::new()))
                .1
                .insert(label.clone());
        }
    }

    // Process bundled components
    let mut bundled = Vec::new();
    for (label, blurb, lic_path, url) in BUNDLED_COMPONENTS {
        bundled.push((*label, *blurb, *url));
        if let Some(path) = lic_path {
            let full_path = Path::new(manifest_dir).join(path);
            if let Ok(txt) = fs::read_to_string(&full_path) {
                let txt = txt.trim().to_string();
                if !txt.is_empty() {
                    let fname = Path::new(path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    texts
                        .entry(txt)
                        .or_insert_with(|| (fname, std::collections::BTreeSet::new()))
                        .1
                        .insert(label.to_string());
                }
            }
        }
    }

    // Build the output document
    let mut lines = Vec::new();
    lines.push("THIRD-PARTY SOFTWARE NOTICES".to_string());
    lines.push("=".repeat(76));
    lines.push(String::new());
    lines.push("punktfunk-webos (https://github.com/punktfunk/client-webos) is licensed under".to_string());
    lines.push("MIT OR Apache-2.0, matching upstream punktfunk (https://git.unom.io/unom/punktfunk).".to_string());
    lines.push("The app links the third-party Rust crates listed below, and bundles or embeds the".to_string());
    lines.push("non-Rust components listed first. Each is distributed under its own permissive".to_string());
    lines.push("license; the full license texts follow the manifest.".to_string());
    lines.push(String::new());
    lines.push("Generated by build.rs — do not edit by hand.".to_string());
    lines.push(String::new());
    lines.push(format!("Total third-party crates: {}", pkgs.len()));
    lines.push(String::new());

    if !bundled.is_empty() {
        lines.push("-".repeat(76));
        lines.push("BUNDLED / EMBEDDED NON-RUST COMPONENTS".to_string());
        lines.push("-".repeat(76));
        for (label, blurb, url) in &bundled {
            lines.push(format!("  {}", label));
            lines.push(format!("      {}", blurb));
            lines.push(format!("      {}", url));
        }
        lines.push(String::new());
    }

    lines.push("-".repeat(76));
    lines.push("MANIFEST (crate version — SPDX license — source)".to_string());
    lines.push("-".repeat(76));
    for pkg in &pkgs {
        let name = pkg["name"].as_str().unwrap_or("");
        let version = pkg["version"].as_str().unwrap_or("");
        let license = if let Some(lic) = pkg["license"].as_str() {
            lic.to_string()
        } else if let Some(lic_file) = pkg["license_file"].as_str() {
            format!("file: {}", lic_file)
        } else {
            "UNKNOWN".to_string()
        };
        let repo = pkg["repository"].as_str().unwrap_or("");
        if repo.is_empty() {
            lines.push(format!("  {} {} — {}", name, version, license));
        } else {
            lines.push(format!("  {} {} — {} — {}", name, version, license, repo));
        }
    }
    lines.push(String::new());

    if !no_text.is_empty() {
        lines.push("-".repeat(76));
        lines.push("Crates whose package did not embed a license file (SPDX + source only)".to_string());
        lines.push("-".repeat(76));
        for pkg in &no_text {
            let name = pkg["name"].as_str().unwrap_or("");
            let version = pkg["version"].as_str().unwrap_or("");
            let license = pkg["license"].as_str().unwrap_or("UNKNOWN");
            let repo = pkg["repository"].as_str().unwrap_or("");
            if repo.is_empty() {
                lines.push(format!("  {} {} — {}", name, version, license));
            } else {
                lines.push(format!("  {} {} — {} — {}", name, version, license, repo));
            }
        }
        lines.push(String::new());
    }

    lines.push("=".repeat(76));
    lines.push("FULL LICENSE TEXTS (deduplicated)".to_string());
    lines.push("=".repeat(76));

    // Sort texts by the first (lowest) crate name in each group, then by text content for determinism
    let mut texts_vec: Vec<_> = texts.into_iter().collect();
    texts_vec.sort_by(|a, b| {
        let a_first = a.1 .1.iter().next().unwrap_or(&String::new()).to_lowercase();
        let b_first = b.1 .1.iter().next().unwrap_or(&String::new()).to_lowercase();
        match a_first.cmp(&b_first) {
            std::cmp::Ordering::Equal => a.0.cmp(&b.0),
            other => other,
        }
    });

    for (txt, (fname, crates)) in &texts_vec {
        lines.push(String::new());
        lines.push("-".repeat(76));
        let crates_str = crates.iter().map(String::as_str).collect::<Vec<_>>().join(", ");
        lines.push(format!("The following license ({}) applies to: {}", fname, crates_str));
        lines.push("-".repeat(76));
        lines.push(txt.clone());
        lines.push(String::new());
    }

    let output = lines.join("\n") + "\n";
    let output_path = Path::new(manifest_dir).join("THIRD-PARTY-NOTICES.txt");
    fs::write(&output_path, output).expect("failed to write THIRD-PARTY-NOTICES.txt");
    eprintln!(
        "wrote THIRD-PARTY-NOTICES.txt: {} crates, {} distinct license texts, {} without embedded text",
        pkgs.len(),
        texts_vec.len(),
        no_text.len()
    );
}

fn find_license_files(pkg_dir: &std::path::Path) -> Vec<(String, String)> {
    const LICENSE_GLOBS: &[&str] = &["license", "licence", "copying", "notice", "unlicense", "copyright"];
    let mut out = Vec::new();

    if let Ok(entries) = std::fs::read_dir(pkg_dir) {
        let mut names: Vec<_> = entries.filter_map(|e| e.ok().map(|entry| entry.path())).collect();
        names.sort();

        for path in names {
            if let Some(name) = path.file_name() {
                let name_str = name.to_string_lossy();
                let name_lower = name_str.to_lowercase();
                let is_license = LICENSE_GLOBS.iter().any(|glob| {
                    name_lower == *glob
                        || name_lower.starts_with(&format!("{}.", glob))
                        || name_lower.starts_with(&format!("{}-", glob))
                        || name_lower.contains(glob)
                });

                if is_license && path.is_file() {
                    if let Ok(txt) = std::fs::read_to_string(&path) {
                        let txt = txt.trim().to_string();
                        if !txt.is_empty() {
                            out.push((name_str.to_string(), txt));
                        }
                    }
                }
            }
        }
    }

    out
}
