use std::io::Write as _;

fn main() {
    slint_build::compile("ui/main.slint").expect("failed to compile Slint UI");

    #[cfg(windows)]
    windows_resources();
}

/// 生成 Windows 资源脚本：图标、版本信息、DPI/长路径清单。
///
/// 版本号来自 Cargo package version，避免 `VERSION`、安装包和 exe 属性三处漂移。
#[cfg(windows)]
fn windows_resources() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    let manifest = std::fs::canonicalize("app.manifest").expect("resolve app.manifest");
    let icon = std::fs::canonicalize("assets/app.ico").expect("resolve app.ico");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    let mut parts = version
        .split(['.', '-', '+'])
        .filter_map(|part| part.parse::<u16>().ok())
        .collect::<Vec<_>>();
    parts.resize(4, 0);
    let numeric = format!("{}, {}, {}, {}", parts[0], parts[1], parts[2], parts[3]);

    let rc = format!(
        r#"#include <winver.h>

#pragma code_page(65001)

1 ICON "{icon}"
1 24 "{manifest}"

1 VERSIONINFO
 FILEVERSION {numeric}
 PRODUCTVERSION {numeric}
 FILEFLAGSMASK 0x3fL
 FILEFLAGS 0x0L
 FILEOS 0x40004L
 FILETYPE 0x1L
 FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Luma contributors"
      VALUE "FileDescription", "Luma Desktop"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "Luma"
      VALUE "LegalCopyright", "Copyright (C) 2026 Luma contributors. GPL-3.0-only."
      VALUE "OriginalFilename", "Luma.exe"
      VALUE "ProductName", "Luma"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = rc_path(&icon),
        manifest = rc_path(&manifest),
        version = version,
        numeric = numeric,
    );

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"))
        .join("luma-resources.rc");
    let mut file = std::fs::File::create(&out).expect("create generated rc");
    let _ = write!(file, "{rc}");

    // 把 icon、版本信息、清单一起写进 exe 资源。
    match embed_resource::compile(&out, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok => {}
        other => panic!("failed to embed Windows resources: {other:?}"),
    }
}

#[cfg(windows)]
fn rc_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
