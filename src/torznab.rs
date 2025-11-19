use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc2822};

#[derive(Debug, Clone)]
pub struct ChannelMetadata {
    pub title: String,
    pub description: String,
    pub site_link: String,
}

#[derive(Debug, Clone)]
pub struct TorznabItem {
    pub title: String,
    pub guid: String,
    pub link: String,
    pub comments: Option<String>,
    pub published: Option<OffsetDateTime>,
    pub size_bytes: u64,
    pub info_hash: Option<String>,
    pub seeders: u32,
    pub leechers: u32,
    pub categories: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct TorznabCategory {
    pub id: u32,
    pub name: &'static str,
    pub subcategories: &'static [TorznabSubCategory],
}

#[derive(Debug, Clone)]
pub struct TorznabSubCategory {
    pub id: u32,
    pub name: &'static str,
}

const TAG: &str = "internal";
const DESC: &str = "Description";

pub const ANIME_CATEGORY: TorznabCategory = TorznabCategory {
    id: 5000,
    name: "TV",
    subcategories: &[TorznabSubCategory {
        id: 5070,
        name: "TV/Anime",
    }],
};

pub const MOVIE_CATEGORY: TorznabCategory = TorznabCategory {
    id: 2000,
    name: "Movies",
    subcategories: &[],
};

pub fn default_categories() -> Vec<TorznabCategory> {
    vec![ANIME_CATEGORY, MOVIE_CATEGORY]
}

#[derive(Debug, Error)]
pub enum TorznabBuildError {
    #[error("failed to build XML document")]
    Xml(#[from] quick_xml::Error),
    #[error("failed to format XML document as UTF-8")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("failed to format timestamp in RFC2822 format")]
    Timestamp(#[from] time::error::Format),
}

pub fn render_caps(metadata: &ChannelMetadata) -> Result<String, TorznabBuildError> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 4);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    writer.write_event(Event::Start(BytesStart::new("caps")))?;

    let mut server = BytesStart::new("server");
    server.push_attribute(("title", metadata.title.as_str()));
    server.push_attribute(("description", metadata.description.as_str()));
    server.push_attribute(("version", env!("CARGO_PKG_VERSION")));
    writer.write_event(Event::Empty(server))?;

    let mut limits = BytesStart::new("limits");
    limits.push_attribute(("default", "100"));
    limits.push_attribute(("max", "100"));
    limits.push_attribute(("min", "1"));
    writer.write_event(Event::Empty(limits))?;

    let mut registration = BytesStart::new("registration");
    registration.push_attribute(("available", "no"));
    registration.push_attribute(("open", "no"));
    writer.write_event(Event::Empty(registration))?;

    writer.write_event(Event::Start(BytesStart::new("searching")))?;

    let mut search_el = BytesStart::new("search");
    search_el.push_attribute(("available", "yes"));
    writer.write_event(Event::Empty(search_el))?;

    let mut tv_search_el = BytesStart::new("tv-search");
    tv_search_el.push_attribute(("available", "yes"));
    tv_search_el.push_attribute(("supportedParams", "tvdbid,season"));
    writer.write_event(Event::Empty(tv_search_el))?;

    let mut movie_search_el = BytesStart::new("movie-search");
    movie_search_el.push_attribute(("available", "yes"));
    movie_search_el.push_attribute(("supportedParams", "tmdbid"));
    writer.write_event(Event::Empty(movie_search_el))?;

    writer.write_event(Event::End(BytesEnd::new("searching")))?;

    writer.write_event(Event::Start(BytesStart::new("categories")))?;

    for category in default_categories() {
        let id_attr = category.id.to_string();
        let mut category_el = BytesStart::new("category");
        category_el.push_attribute(("id", id_attr.as_str()));
        category_el.push_attribute(("name", category.name));

        if category.subcategories.len() > 0 {
            writer.write_event(Event::Start(category_el))?;

            for sub in category.subcategories {
                let sub_id = sub.id.to_string();
                let mut sub_el = BytesStart::new("subcat");
                sub_el.push_attribute(("id", sub_id.as_str()));
                sub_el.push_attribute(("name", sub.name));
                writer.write_event(Event::Empty(sub_el))?;
            }

            writer.write_event(Event::End(BytesEnd::new("category")))?;
        } else {
            writer.write_event(Event::Empty(category_el))?;
        }
    }
    writer.write_event(Event::End(BytesEnd::new("categories")))?;

    writer.write_event(Event::Start(BytesStart::new("tags")))?;
    {
        let mut tag_el = BytesStart::new("tag");
        tag_el.push_attribute(("name", TAG));
        tag_el.push_attribute(("description", DESC));
        writer.write_event(Event::Empty(tag_el))?;
    }
    writer.write_event(Event::End(BytesEnd::new("tags")))?;

    writer.write_event(Event::End(BytesEnd::new("caps")))?;

    Ok(String::from_utf8(writer.into_inner())?)
}

pub fn render_feed(
    metadata: &ChannelMetadata,
    items: &[TorznabItem],
    _offset: usize,
    _total: usize,
) -> Result<String, TorznabBuildError> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    let mut rss = BytesStart::new("rss");
    rss.push_attribute(("version", "2.0"));
    rss.push_attribute(("xmlns:torznab", "http://torznab.com/schemas/2015/feed"));
    writer.write_event(Event::Start(rss))?;

    writer.write_event(Event::Start(BytesStart::new("channel")))?;
    write_text_element(&mut writer, "title", &metadata.title)?;
    write_text_element(&mut writer, "description", &metadata.description)?;
    write_text_element(&mut writer, "link", &metadata.site_link)?;

    for item in items.iter() {
        writer.write_event(Event::Start(BytesStart::new("item")))?;
        write_text_element(&mut writer, "title", &item.title)?;
        write_text_element(&mut writer, "guid", &item.guid)?;
        write_text_element(&mut writer, "link", &item.link)?;

        if let Some(comments) = item.comments.as_deref() {
            write_text_element(&mut writer, "comments", comments)?;
        }

        if let Some(published) = item.published {
            let formatted = published.format(&Rfc2822)?;
            write_text_element(&mut writer, "pubDate", &formatted)?;
        }

        write_text_element(&mut writer, "size", &item.size_bytes.to_string())?;

        if let Some(info_hash) = item.info_hash.as_deref() {
            write_text_element(&mut writer, "infohash", info_hash)?;
        }

        let mut enclosure = BytesStart::new("enclosure");
        enclosure.push_attribute(("url", item.link.as_str()));
        enclosure.push_attribute(("type", "application/x-bittorrent"));
        enclosure.push_attribute(("length", item.size_bytes.to_string().as_str()));
        writer.write_event(Event::Empty(enclosure))?;

        if item.categories.is_empty() {
            write_attr(&mut writer, "category", &ANIME_CATEGORY.id.to_string())?;
            if let Some(sub) = ANIME_CATEGORY.subcategories.first() {
                write_attr(&mut writer, "category", &sub.id.to_string())?;
            }
        } else {
            for category_id in &item.categories {
                write_attr(&mut writer, "category", &category_id.to_string())?;
            }
        }
        write_attr(&mut writer, "seeders", &item.seeders.to_string())?;
        write_attr(&mut writer, "leechers", &item.leechers.to_string())?;
        write_attr(&mut writer, "tag", TAG)?;

        writer.write_event(Event::End(BytesEnd::new("item")))?;
    }

    writer.write_event(Event::End(BytesEnd::new("channel")))?;
    writer.write_event(Event::End(BytesEnd::new("rss")))?;

    Ok(String::from_utf8(writer.into_inner())?)
}

fn write_text_element(
    writer: &mut Writer<Vec<u8>>,
    name: &str,
    value: &str,
) -> Result<(), quick_xml::Error> {
    writer.write_event(Event::Start(BytesStart::new(name)))?;
    writer.write_event(Event::Text(BytesText::new(value)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))?;
    Ok(())
}

fn write_attr(
    writer: &mut Writer<Vec<u8>>,
    name: &str,
    value: &str,
) -> Result<(), quick_xml::Error> {
    let mut attr = BytesStart::new("torznab:attr");
    attr.push_attribute(("name", name));
    attr.push_attribute(("value", value));
    writer.write_event(Event::Empty(attr))?;
    Ok(())
}
