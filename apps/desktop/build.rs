use std::{env, error::Error, fs::File, path::PathBuf};

use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
use image::{DynamicImage, GenericImage, ImageBuffer, Rgba, imageops::FilterType};
use slint_build::{CompilerConfiguration, EmbedResourcesKind};

const APPLICATION_NAME: &str = "Equestrian Dream Launcher";
const EXECUTABLE_NAME: &str = "EquestrianDreamLauncher.exe";
const ICON_SOURCE: &str = "../../assets/decorations/project-horse.png";
const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed={ICON_SOURCE}");

    let slint_config = CompilerConfiguration::new().embed_resources(EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/main.slint", slint_config)?;

    #[cfg(windows)]
    compile_windows_resources()?;

    Ok(())
}

#[cfg(windows)]
fn compile_windows_resources() -> Result<(), Box<dyn Error>> {
    let out_directory = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let icon_path = out_directory.join("EquestrianDreamLauncher.ico");
    generate_windows_icon(&icon_path)?;

    let version = env::var("CARGO_PKG_VERSION")?;
    let windows_version = format!("{version}.0");
    let numeric_version = windows_numeric_version(&version)?;
    let icon_path = icon_path
        .to_str()
        .ok_or("generated icon path is not valid Unicode")?;

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(icon_path)
        .set("ProductName", APPLICATION_NAME)
        .set("FileDescription", APPLICATION_NAME)
        .set("InternalName", "EquestrianDreamLauncher")
        .set("OriginalFilename", EXECUTABLE_NAME)
        .set("ProductVersion", &windows_version)
        .set("FileVersion", &windows_version)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, numeric_version)
        .set_version_info(winresource::VersionInfo::FILEVERSION, numeric_version)
        .compile()?;

    Ok(())
}

#[cfg(windows)]
fn generate_windows_icon(output_path: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let source = image::open(ICON_SOURCE)?.into_rgba8();
    let mut icon = IconDir::new(ResourceType::Icon);

    for size in ICON_SIZES {
        let scale = (size as f32 / source.width() as f32).min(size as f32 / source.height() as f32);
        let width = ((source.width() as f32 * scale).round() as u32).max(1);
        let height = ((source.height() as f32 * scale).round() as u32).max(1);
        let resized = DynamicImage::ImageRgba8(source.clone())
            .resize_exact(width, height, FilterType::Lanczos3)
            .into_rgba8();
        let mut square = ImageBuffer::from_pixel(size, size, Rgba([0, 0, 0, 0]));
        square.copy_from(&resized, (size - width) / 2, (size - height) / 2)?;
        let image = IconImage::from_rgba_data(size, size, square.into_raw());
        icon.add_entry(IconDirEntry::encode(&image)?);
    }

    icon.write(File::create(output_path)?)?;
    Ok(())
}

#[cfg(windows)]
fn windows_numeric_version(version: &str) -> Result<u64, Box<dyn Error>> {
    let mut components = version.split('.');
    let major = parse_version_component(components.next(), "major")?;
    let minor = parse_version_component(components.next(), "minor")?;
    let patch = parse_version_component(components.next(), "patch")?;
    if components.next().is_some() {
        return Err(format!("unsupported package version {version}").into());
    }

    Ok((u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16))
}

#[cfg(windows)]
fn parse_version_component(component: Option<&str>, name: &str) -> Result<u16, Box<dyn Error>> {
    component
        .ok_or_else(|| format!("package version is missing its {name} component").into())
        .and_then(|value| {
            value
                .parse::<u16>()
                .map_err(|error| format!("invalid {name} version component: {error}").into())
        })
}
