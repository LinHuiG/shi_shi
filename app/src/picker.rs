//! User-initiated native selection. Paths are returned without opening files or
//! changing configuration; cancellation leaves the editor's draft untouched.
use serde::Deserialize;
use serde_json::Value;

#[derive(Default, Deserialize)]
#[cfg_attr(not(windows), allow(dead_code))]
#[serde(rename_all = "snake_case")]
enum Kind {
    #[default]
    File,
    Directory,
    Plugin,
    Manifest,
}
#[derive(Deserialize)]
#[cfg_attr(not(windows), allow(dead_code))]
#[serde(deny_unknown_fields)]
struct Filter {
    name: String,
    extensions: Vec<String>,
}
#[derive(Default, Deserialize)]
#[cfg_attr(not(windows), allow(dead_code))]
#[serde(default, deny_unknown_fields)]
struct Options {
    #[serde(rename = "kind")]
    title: String,
    #[serde(rename = "type")]
    kind: Kind,
    default_path: String,
    filters: Vec<Filter>,
}
impl Options {
    #[cfg_attr(not(windows), allow(dead_code))]
    fn parse(value: &Value) -> Result<Self, String> {
        let options: Self = serde_json::from_value(value.clone()).map_err(|_| "文件选择参数无效")?;
        if options.title.chars().count() > 100
            || options.title.contains('\0')
            || options.default_path.len() > 32_768
            || options.default_path.contains('\0')
            || options.filters.len() > 8
            || options.filters.iter().any(|filter| {
                filter.name.is_empty()
                    || filter.name.chars().count() > 100
                    || filter.name.contains('\0')
                    || filter.extensions.is_empty()
                    || filter.extensions.len() > 16
                    || filter.extensions.iter().any(|extension| {
                        extension.is_empty()
                            || extension.len() > 16
                            || !extension.bytes().all(|c| c.is_ascii_alphanumeric())
                    })
            })
        {
            return Err("文件选择参数超出范围".into());
        }
        Ok(options)
    }
}

#[cfg(windows)]
pub fn pick(value: &Value) -> Result<Value, String> {
    use std::path::PathBuf;
    let options = Options::parse(value)?;
    let title = if options.title.is_empty() { "文件" } else { &options.title };
    let mut dialog = rfd::FileDialog::new().set_title(format!("选择{title}"));
    if !options.default_path.is_empty() {
        let path = PathBuf::from(&options.default_path);
        if matches!(options.kind, Kind::Directory) || path.is_dir() {
            dialog = dialog.set_directory(path);
        } else {
            if let Some(parent) = path.parent() {
                dialog = dialog.set_directory(parent);
            }
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                dialog = dialog.set_file_name(name);
            }
        }
    }
    for filter in options.filters {
        dialog = dialog.add_filter(filter.name, &filter.extensions);
    }
    let selected = match options.kind {
        Kind::Directory => dialog.pick_folder(),
        Kind::Plugin => dialog.add_filter("插件包或描述", &["zip", "json"]).pick_file(),
        Kind::Manifest => dialog.add_filter("插件描述", &["json"]).pick_file(),
        Kind::File => dialog.pick_file(),
    };
    Ok(serde_json::json!(selected.map(|path| path.to_string_lossy().into_owned())))
}

#[cfg(test)]
mod tests;
