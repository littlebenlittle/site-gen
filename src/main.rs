#[macro_use]
extern crate handlebars;
use anyhow::{bail, Context, Result};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use serde_json::json;

handlebars_helper!(spacer: |sm: u8, md: u8, lg: u8| {
    if sm > 12 || md > 12 || lg > 12 {
        format!("<div><p>w3 column cannot exceed l12</p><div>")
    } else {
        format!("<div class=\"w3-container w3-content w3-col s{sm} m{md} l{lg}\" aria-hidden=\"true\"></div>")
    }
});

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    ///path to directory where compiled site content will be written
    target: String,
    ///path to directory with site content
    source: String,
    ///path to directory with template files
    templates: String,
    ///path to directory with blog files
    blog: String,
}

fn main() -> Result<()> {
    env_logger::init();

    let config: Config = {
        let path = std::fs::File::open("config.yaml").context("could not open config")?;
        let data = std::io::BufReader::new(path);
        serde_yaml::from_reader(data)?
    };

    log::info!("initializing handlebars");
    let mut handlebars = Handlebars::new();
    handlebars.register_helper("spacer", Box::new(spacer));

    log::info!("registering templates");
    register_templates_dir(PathBuf::from(&config.templates), &mut handlebars)?;

    log::info!("processing blog posts");
    let post_metas: Vec<JsonMap> = process_blog_posts(PathBuf::from(&config.blog))?;
    let mut globals = serde_json::Map::new();
    globals.insert(String::from("posts"), json!(post_metas));

    log::info!("compiling site");
    let site = compile_dir(PathBuf::from(&config.source), &globals, handlebars)?;

    log::info!("writing site to filesystem");
    emit_directory(site, PathBuf::from(&config.target))?;

    Ok(())
}

type Directory = Vec<(String, Node)>;

#[derive(Debug)]
enum Node {
    Page(String),
    Dir(Directory),
}

fn compile_dir<T: AsRef<std::path::Path>>(
    path: T,
    globals: &JsonMap,
    handlebars: Handlebars,
) -> Result<Directory> {
    let path = PathBuf::from(path.as_ref());
    if !path.is_dir() {
        bail!("not a directory: {path:?}");
    }
    log::debug!("processing directory: {path:?}");
    let mut directory: Directory = vec![];
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let entry_path: PathBuf = entry.path();
        let file_name = get_file_name(&entry_path);
        if file_name.starts_with("_") {
            log::info!("ignoring path with leading underscore: {entry_path:?}");
            continue;
        }
        if meta.is_file() {
            log::debug!("processing file: {entry_path:?}");
            if let Some((html, out_name)) = compile_file(&entry_path, globals, &handlebars)? {
                directory.push((out_name, Node::Page(html)));
            }
        } else if meta.is_dir() {
            let dir = compile_dir(&entry_path, globals, handlebars.clone())?;
            directory.push((file_name.to_owned(), Node::Dir(dir)));
        } else {
            log::debug!("neither file nor directory; skipping");
        }
    }
    Ok(directory)
}

fn compile_file(
    path: &PathBuf,
    globals: &JsonMap,
    handlebars: &Handlebars,
) -> Result<Option<(String, String)>> {
    match get_file_ext(path) {
        "md" => Ok(compile_markdown(path, globals, handlebars)?),
        _ => {
            log::debug!("unhandled file extension for {path:?}");
            Ok(None)
        }
    }
}

fn compile_markdown(
    path: &PathBuf,
    globals: &JsonMap,
    handlebars: &Handlebars,
) -> Result<Option<(String, String)>> {
    let (fm, md): (JsonMap, String) = split_frontmatter(path)?;
    let mut fm = replace_globals(fm, globals);
    let tmpl_name: String = fm
        .get("template")
        .expect("expected fronmatter to contain template name")
        .as_str()
        .expect("expected template name to be string")
        .to_owned();
    let content = markdown::to_html(&md);
    fm.insert(String::from("content"), json!(content));
    let html: String = handlebars.render(&tmpl_name, &fm)?;
    let mut out_name = get_file_stem(path).to_owned();
    out_name.push_str(".html");
    return Ok(Some((html, out_name)));
}

fn replace_globals(obj: JsonMap, globals: &JsonMap) -> JsonMap {
    let mut new_obj = obj.clone();
    for (key, val) in obj {
        if let Some(val) = val.as_str() {
            if val.starts_with("@") {
                let gkey = val[1..val.len()].to_string();
                if let Some(val) = globals.get(&gkey) {
                    new_obj.insert(key.to_string(), val.clone());
                }
            }
        }
    }
    new_obj
}

type JsonMap = serde_json::Map<String, serde_json::Value>;

fn split_frontmatter(path: &PathBuf) -> Result<(JsonMap, String)> {
    use extract_frontmatter::{config::Splitter, Extractor};
    let fm_extractor = Extractor::new(Splitter::EnclosingLines("---"));
    let data = std::fs::read_to_string(path)?;
    let (fm, data) = fm_extractor.extract(&data);
    let options: JsonMap = {
        let options: serde_yaml::Value = serde_yaml::from_str(&fm)?;
        let options: serde_json::Value = json!(options);
        options
            .as_object()
            .expect("expected fronmatter to be mapping")
            .clone()
    };
    Ok((options, data.to_owned()))
}

fn emit_directory<T: AsRef<std::path::Path>>(dir: Directory, target: T) -> Result<()> {
    for (path, node) in dir {
        let mut target = PathBuf::from(target.as_ref());
        target.push(path);
        if target.is_file() {
            std::fs::remove_file(&target)?
        }
        if target.is_dir() {
            std::fs::remove_dir_all(&target)?
        }
        log::debug!("emitting: {target:?}");
        match node {
            Node::Page(html) => {
                log::debug!("emitting html");
                std::fs::write(target, html)?
            }
            Node::Dir(dir) => {
                log::debug!("emitting directory");
                std::fs::create_dir(&target)?;
                emit_directory(dir, &target)?;
            }
        }
    }
    Ok(())
}

fn register_templates_dir<T: AsRef<std::path::Path>>(
    path: T,
    handlebars: &mut Handlebars,
) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry: std::fs::DirEntry = entry?;
        let path: std::path::PathBuf = entry.path();
        let metadata = entry.metadata().expect("couldn't get metadata for path");
        if metadata.is_file() {
            if get_file_ext(&path) == "hbs" {
                handlebars.register_template_file(get_file_stem(&path), &path)?;
            } else {
                log::info!("skipping {path:?} due to extension");
            }
        } else if metadata.is_dir() {
            register_templates_dir(path, handlebars)?;
        }
    }
    Ok(())
}

fn process_blog_posts<T: AsRef<std::path::Path>>(blog_dir: T) -> Result<Vec<JsonMap>> {
    let mut post_metas: Vec<JsonMap> = vec![];
    for entry in std::fs::read_dir(blog_dir)? {
        let path = entry?.path();
        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}-").unwrap();
        if re.is_match(get_file_name(&path)) {
            let (mut fm, _): (JsonMap, _) = split_frontmatter(&path)?;
            let mut out_name = get_file_stem(&path).to_owned();
            out_name.push_str(".html");
            fm.insert("link".to_owned(), json!(out_name));
            post_metas.push(fm)
        }
    }
    post_metas.sort_by(|a: &JsonMap, b: &JsonMap| get_date(b).cmp(get_date(a)));
    Ok(post_metas)
}

fn get_file_name<'a>(path: &'a PathBuf) -> &'a str {
    path.file_name()
        .expect("couldn't get file_name")
        .to_str()
        .expect("couldn't convert file_name to string")
}

fn get_file_stem<'a>(path: &'a PathBuf) -> &'a str {
    path.file_stem()
        .expect("couldn't get file_stem")
        .to_str()
        .expect("couldn't convert file_stem to string")
}

fn get_file_ext<'a>(path: &'a PathBuf) -> &'a str {
    path.extension().expect("couldn't extract file extension")
        .to_str()
        .expect("could not convert file extension to string")
}

fn get_date<'a>(v: &'a JsonMap) -> &'a str {
    v.get("date")
        .expect("expected frontmatter to contain date")
        .as_str()
        .expect("date must be a string")
}
