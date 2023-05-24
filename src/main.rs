#[macro_use]
extern crate handlebars;
use self::file_helpers::*;
use anyhow::{bail, Context, Result};
use handlebars::{Handlebars, JsonValue};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use clap::Parser;

use serde_json::json;

use regex::Regex;

type JsonMap = serde_json::Map<String, serde_json::Value>;

type Directory = Vec<(String, Node)>;

#[derive(Debug)]
enum Node {
    Page(String),
    Dir(Directory),
}

handlebars_helper!(lt: |left: u16, right: u16| {
    left < right
});

#[derive(Parser)]
struct Cli {
    /// path to config file
    #[clap(long, default_value = "config.yaml")]
    config: String,
}

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
    ///paths to exclude
    ignore: Option<Vec<String>>,
}

fn main() -> Result<()> {
    env_logger::init();

    let args = Cli::parse();
    let config: Config = {
        let path = std::fs::File::open(&args.config).context("could not open config")?;
        let data = std::io::BufReader::new(path);
        serde_yaml::from_reader(data)?
    };

    log::info!("initializing handlebars");
    let mut handlebars = Handlebars::new();
    handlebars.register_helper("spacer", Box::new(lt));

    log::info!("registering templates");
    register_templates_dir(PathBuf::from(&config.templates), &mut handlebars)?;

    log::info!("processing blog posts");
    let globals = json!({
        "posts": process_blog_posts(PathBuf::from(&config.blog))?
    });

    log::info!("compiling site");
    let mut ignore = Vec::<String>::new();
    ignore.append(&mut config.ignore.unwrap_or_default());
    ignore.push(config.templates);
    let site = compile_dir(PathBuf::from(&config.source), &globals, &ignore, handlebars)?;

    log::info!("writing site to filesystem");
    emit_directory(site, PathBuf::from(&config.target))?;

    Ok(())
}

fn compile_dir<T: AsRef<std::path::Path>>(
    path: T,
    globals: &JsonValue,
    ignore: &Vec<String>,
    handlebars: Handlebars,
) -> Result<Directory> {
    let path = PathBuf::from(path.as_ref()).canonicalize()?;
    if !path.is_dir() {
        bail!("not a directory: {path:?}");
    }
    log::debug!("{}", path.to_str().unwrap());
    log::debug!("processing directory: {path:?}");
    let mut directory: Directory = vec![];
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let entry_path: PathBuf = entry.path();
        let file_name = get_file_name(&entry_path)?;
        if file_name.starts_with("_") {
            log::debug!("ignoring path with leading underscore: {entry_path:?}");
            continue;
        }
        log::debug!("{:?}", ignore);
        if ignore.contains(&file_name.to_owned()) {
            log::debug!("ignoring path list as ingored in config: {entry_path:?}");
            continue;
        }
        if meta.is_file() {
            log::debug!("processing file: {entry_path:?}");
            if let Some((html, out_name)) = compile_file(&entry_path, globals, &handlebars)? {
                directory.push((out_name, Node::Page(html)));
            }
        } else if meta.is_dir() {
            let dir = compile_dir(&entry_path, globals, ignore, handlebars.clone())?;
            directory.push((file_name.to_owned(), Node::Dir(dir)));
        } else {
            log::debug!("neither file nor directory; skipping");
        }
    }
    Ok(directory)
}

fn compile_file(
    path: impl AsRef<Path>,
    globals: &JsonValue,
    handlebars: &Handlebars,
) -> Result<Option<(String, String)>> {
    let path = path.as_ref();
    match get_file_ext(path)? {
        "md" => Ok(compile_markdown(path, globals, handlebars)?),
        _ => {
            log::debug!("unhandled file extension for {path:?}");
            Ok(None)
        }
    }
}

fn compile_markdown(
    path: impl AsRef<Path>,
    globals: &JsonValue,
    handlebars: &Handlebars,
) -> Result<Option<(String, String)>> {
    let path = path.as_ref();
    let (fm, mut md): (JsonValue, String) = split_frontmatter(path)?;
    let mut fm = replace_globals(fm, globals);
    md = replace_uuid_links(md, globals).context(format!("processing {}", path.display()))?;
    let tmpl_name: String = fm
        .get("template")
        .expect("expected frontmatter to contain template name")
        .as_str()
        .expect("expected template name to be string")
        .to_owned();
    let content = markdown::to_html(&md);
    fm.insert(String::from("content"), json!(content));
    let html: String = handlebars.render(&tmpl_name, &fm).context("{path:?}")?;
    let mut out_name = get_file_stem(path)?.to_owned();
    out_name.push_str(".html");
    return Ok(Some((html, out_name)));
}

fn replace_globals(obj: JsonValue, globals: &JsonValue) -> JsonMap {
    let obj = obj.as_object().expect("obj to be mapping");
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

fn replace_uuid_links(mut text: String, globals: &JsonValue) -> Result<String> {
    let mut new_text = text.clone();
    let re = Regex::new(r"\[[^\]]+\]\(:([a-zA-Z0-9]+)\)").unwrap();
    let mut offset = 0;
    loop {
        let link = match re.captures(&text) {
            Some(cap) => cap,
            None => break,
        };
        let url = {
            let posts = globals
                .get("posts")
                .expect("globals to have posts key")
                .as_array()
                .expect("posts to be an array");
            let mut url = "";
            for post in posts {
                let id = post
                    .get("id")
                    .expect("post meta to have id")
                    .as_str()
                    .expect("post id to be string");
                let post_title = post.get("title").expect("post to have title key");
                log::debug!("post {} has uuid {}", post_title, id);
                if id == &JsonValue::from(&link[1]) {
                    url = post
                        .get("link")
                        .expect("post to have link key")
                        .as_str()
                        .expect("link to be a string");
                }
            }
            if url == "" {
                bail!("uuid in text ({}) should correspond to a post", &link[1])
            }
            url
        };
        let uuid_part = link.get(1).expect("link to have uuid");
        new_text.replace_range(
            (offset + uuid_part.start() - 1)..(offset + uuid_part.end()),
            &url,
        );
        offset += uuid_part.start() + url.len() - 1;
        text = text[uuid_part.end()..].into();
    }
    Ok(new_text)
}

fn split_frontmatter(path: impl AsRef<Path>) -> Result<(JsonValue, String)> {
    use extract_frontmatter::{config::Splitter, Extractor};
    let fm_extractor = Extractor::new(Splitter::EnclosingLines("---"));
    let data = std::fs::read_to_string(path)?;
    let (fm, data) = fm_extractor.extract(&data);
    let options = serde_yaml::from_str::<JsonValue>(&fm)?;
    Ok((options, data.to_owned()))
}

fn emit_directory(dir: Directory, target: impl AsRef<Path>) -> Result<()> {
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
                std::fs::create_dir_all(&target)?;
                emit_directory(dir, &target)?;
            }
        }
    }
    Ok(())
}

fn register_templates_dir(path: impl AsRef<Path>, handlebars: &mut Handlebars) -> Result<()> {
    if !path.as_ref().is_dir() {
        log::info!("templates directory doesn't exist");
        return Ok(())
    }
    for entry in std::fs::read_dir(path)? {
        let entry: std::fs::DirEntry = entry?;
        let path: std::path::PathBuf = entry.path();
        let metadata = entry.metadata().expect("couldn't get metadata for path");
        if metadata.is_file() {
            if get_file_ext(&path)? == "hbs" {
                handlebars.register_template_file(get_file_stem(&path)?, &path)?;
            } else {
                log::info!("skipping {path:?} due to extension");
            }
        } else if metadata.is_dir() {
            register_templates_dir(path, handlebars)?;
        }
    }
    Ok(())
}

fn process_blog_posts(blog_dir: impl AsRef<Path>) -> Result<Vec<JsonValue>> {
    let blog_dir = blog_dir.as_ref().canonicalize()?;
    let mut posts = Vec::new();
    process_blog_posts_dir(&blog_dir, &blog_dir, &mut posts)?;
    posts.sort_by(|a: &JsonValue, b: &JsonValue| get_date(b).cmp(get_date(a)));
    Ok(posts)
}

fn process_blog_posts_dir(
    path: impl AsRef<Path>,
    blog_dir: impl AsRef<Path>,
    posts: &mut Vec<JsonValue>,
) -> Result<()> {
    let dir_path = path.as_ref();
    let blog_dir = blog_dir.as_ref();
    if get_file_name(dir_path)?.starts_with("_") {
        log::info!(
            "skipping directory with leading underscore: {}",
            dir_path.display()
        );
        return Ok(());
    }
    for entry in std::fs::read_dir(dir_path)? {
        let path = entry?.path();
        if path.is_dir() {
            process_blog_posts_dir(path, &blog_dir, posts)?;
        } else {
            let filename = get_file_name(&path)?;
            if filename.starts_with("_") {
                log::info!("skipping file with leading underscore: {}", path.display());
                continue;
            }
            if filename == "index.md" {
                log::info!("skipping index");
                continue;
            }
            if get_file_ext(&path)? == "md" {
                let (mut fm, _): (JsonValue, _) = split_frontmatter(&path)?;
                let fm = fm.as_object_mut().unwrap();
                ensure_key(fm, "date", "str")
                    .context(format!("processing file {}", path.display()))?;
                let mut out_name = get_file_stem(&path)?.to_owned();
                out_name.push_str(".html");
                let mut link = PathBuf::new();
                link.push("/blog");
                link.push(path.strip_prefix(&blog_dir)?.parent().unwrap());
                link.push(&out_name);
                log::debug!("link is {link:?}");
                fm.insert("link".to_owned(), json!(link));
                posts.push(json! {fm});
            }
        }
    }
    Ok(())
}

fn ensure_key(v: &JsonMap, key: &str, kind: &'static str) -> Result<()> {
    match v.get(key) {
        None => bail!("value does not contain key `{key}`"),
        Some(val) => match kind {
            "str" => match val.as_str() {
                None => bail!("expected value of `{key}` to be of type `{kind}`"),
                Some(_) => Ok(()),
            },
            _ => panic!("invalid kind `{kind}`"),
        },
    }
}

mod file_helpers {
    use super::{bail, Result};
    use std::path::Path;

    pub fn get_file_name<'a>(path: &'a Path) -> Result<&'a str> {
        match path.file_name() {
            None => bail!("couldn't get file name"),
            Some(stem) => match stem.to_str() {
                None => bail!("could not convert file name to string"),
                Some(s) => Ok(s),
            },
        }
    }

    pub fn get_file_stem<'a>(path: &'a Path) -> Result<&'a str> {
        match path.file_stem() {
            None => bail!("couldn't get file sterm"),
            Some(stem) => match stem.to_str() {
                None => bail!("could not convert file stem to string"),
                Some(s) => Ok(s),
            },
        }
    }

    pub fn get_file_ext<'a>(path: &'a Path) -> Result<&'a str> {
        match path.extension() {
            None => bail!("couldn't extract file extension"),
            Some(ext) => match ext.to_str() {
                None => bail!("could not convert file extension to string"),
                Some(s) => Ok(s),
            },
        }
    }

    pub fn get_date<'a>(v: &'a serde_json::Value) -> &'a str {
        v.get("date").unwrap().as_str().unwrap()
    }
}

fn expand_shorthand(mut text: &str, table: JsonValue) -> String {
    let table = table.as_object().expect("shorthand table to be object");
    let mut new_text = text.to_owned();
    for (from, to) in table {
        let from = from.as_str();
        let to = to.as_str().expect("shorthand target to be a string");
        let re = Regex::new(from).expect(&format!("a valid regex. Got \"{from}\""));
        let mut offset = 0;
        while let Some(mat) = re.find(text) {
            let start = offset + mat.start();
            let end = offset + mat.end();
            new_text.replace_range(start..end, "");
            new_text.insert_str(mat.start(), to);
            offset += start + to.len() - 1;
            text = text[end..].into();
        }
    }
    new_text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_replace_uuid_links() -> Result<()> {
        let text = String::from("Here is [a uuid link](:abc123ABC987) for you!");
        let globals = json!({
            "posts":  {
                "abc123ABC987": {
                    "title": "My Post",
                    "link": "/blog/my_post.html"
                }
            }
        });
        let new_text = replace_uuid_links(text, &globals)?;
        assert_eq!(
            new_text,
            "Here is [a uuid link](/blog/my_post.html) for you!"
        );
        Ok(())
    }

    #[test]
    fn can_replace_multiple_uuid_links() -> Result<()> {
        let mut text = String::from("Here is [a uuid link](:abc123ABC987) for you!");
        text.push_str("\n\n");
        text.push_str("Here is [another uuid link](:xyz456XYZ751) for you!");
        let globals = json!({
            "posts":  {
                "abc123ABC987": {
                    "title": "My Post",
                    "link": "/blog/my_post.html",
                },
                "xyz456XYZ751": {
                    "title": "My Other Post",
                    "link": "/blog/my_other_post.html",
                },
            }
        });
        let new_text = replace_uuid_links(text, &globals)?;
        let mut expected_text = String::from("Here is [a uuid link](/blog/my_post.html) for you!");
        expected_text.push_str("\n\n");
        expected_text.push_str("Here is [another uuid link](/blog/my_other_post.html) for you!");
        assert_eq!(new_text, expected_text);
        Ok(())
    }

    #[test]
    fn can_expand_shorthand_sequences() {
        let mut text = String::from("This is a longdash--it should expand.");
        text.push_str("\n\n");
        text.push_str("This [looks like a longdash](http://example.com/link--thing) and it should NOT expand.");
        let mut expect = String::from("This is a longdash&#151;it should expand.");
        expect.push_str("\n\n");
        expect.push_str("This [looks like a longdash](http://example.com/link--thing) and it should NOT expand.");
        let got: String = expand_shorthand(
            &text,
            json!({
                "--": "&#151;"
            }),
        );
        assert_eq!(got, expect)
    }
}
