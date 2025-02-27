use crate::{builtins::BUILTINS, Flags};
use percent_encoding::percent_decode_str;
use std::{
  borrow::Cow,
  path::{is_separator, Path, PathBuf},
};
use url::Url;

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum SpecifierType {
  Esm,
  Cjs,
  Url,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", content = "value")]
pub enum SpecifierError {
  EmptySpecifier,
  InvalidPackageSpecifier,
  #[serde(serialize_with = "serialize_url_error")]
  UrlError(url::ParseError),
  InvalidFileUrl,
}

impl From<url::ParseError> for SpecifierError {
  fn from(value: url::ParseError) -> Self {
    SpecifierError::UrlError(value)
  }
}

fn serialize_url_error<S>(value: &url::ParseError, serializer: S) -> Result<S::Ok, S::Error>
where
  S: serde::Serializer,
{
  use serde::Serialize;
  value.to_string().serialize(serializer)
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum Specifier<'a> {
  Relative(Cow<'a, Path>),
  Absolute(Cow<'a, Path>),
  Tilde(Cow<'a, Path>),
  Hash(Cow<'a, str>),
  Package(Cow<'a, str>, Cow<'a, str>),
  Builtin(Cow<'a, str>),
  Url(&'a str),
}

impl<'a> Specifier<'a> {
  pub fn parse(
    specifier: &'a str,
    specifier_type: SpecifierType,
    flags: Flags,
  ) -> Result<(Specifier<'a>, Option<&'a str>), SpecifierError> {
    if specifier.is_empty() {
      return Err(SpecifierError::EmptySpecifier);
    }

    Ok(match specifier.as_bytes()[0] {
      b'.' => {
        let specifier = if specifier.starts_with("./") {
          &specifier[2..]
        } else {
          specifier
        };
        let (path, query) = decode_path(specifier, specifier_type);
        (Specifier::Relative(path), query)
      }
      b'~' => {
        let mut specifier = &specifier[1..];
        if !specifier.is_empty() && is_separator(specifier.as_bytes()[0] as char) {
          specifier = &specifier[1..];
        }
        let (path, query) = decode_path(specifier, specifier_type);
        (Specifier::Tilde(path), query)
      }
      b'/' => {
        if specifier.starts_with("//") && specifier_type == SpecifierType::Url {
          // A protocol-relative URL, e.g `url('//example.com/foo.png')`.
          (Specifier::Url(specifier), None)
        } else {
          let (path, query) = decode_path(specifier, specifier_type);
          (Specifier::Absolute(path), query)
        }
      }
      b'#' => (Specifier::Hash(Cow::Borrowed(&specifier[1..])), None),
      _ => {
        // Bare specifier.
        match specifier_type {
          SpecifierType::Url | SpecifierType::Esm => {
            // Check if there is a scheme first.
            if let Ok((scheme, rest)) = parse_scheme(specifier) {
              let (path, rest) = parse_path(rest);
              let (query, _) = parse_query(rest);
              match scheme.as_ref() {
                "npm" if flags.contains(Flags::NPM_SCHEME) => {
                  if BUILTINS.contains(&path.as_ref()) {
                    return Ok((Specifier::Builtin(Cow::Borrowed(path)), None));
                  }

                  (
                    parse_package(percent_decode_str(path).decode_utf8_lossy())?,
                    query,
                  )
                }
                "node" => {
                  // Node does not URL decode or support query params here.
                  // See https://github.com/nodejs/node/issues/39710.
                  (Specifier::Builtin(Cow::Borrowed(path)), None)
                }
                "file" => {
                  // Fully parsing file urls is somewhat complex, so use the url crate for this.
                  let url = Url::parse(specifier)?;
                  (
                    Specifier::Absolute(Cow::Owned(
                      url
                        .to_file_path()
                        .map_err(|_| SpecifierError::InvalidFileUrl)?,
                    )),
                    query,
                  )
                }
                _ => (Specifier::Url(specifier), None),
              }
            } else {
              // If not, then parse as an npm package if this is an ESM specifier,
              // otherwise treat this as a relative path.
              let (path, rest) = parse_path(specifier);
              if specifier_type == SpecifierType::Esm {
                if BUILTINS.contains(&path.as_ref()) {
                  return Ok((Specifier::Builtin(Cow::Borrowed(path)), None));
                }

                let (query, _) = parse_query(rest);
                (
                  parse_package(percent_decode_str(path).decode_utf8_lossy())?,
                  query,
                )
              } else {
                let (path, query) = decode_path(specifier, specifier_type);
                (Specifier::Relative(path), query)
              }
            }
          }
          SpecifierType::Cjs => {
            if BUILTINS.contains(&specifier.as_ref()) {
              (Specifier::Builtin(Cow::Borrowed(specifier)), None)
            } else {
              #[cfg(windows)]
              if !flags.contains(Flags::ABSOLUTE_SPECIFIERS) {
                let path = Path::new(specifier);
                if path.is_absolute() {
                  return Ok((Specifier::Absolute(Cow::Borrowed(path)), None));
                }
              }

              (parse_package(Cow::Borrowed(specifier))?, None)
            }
          }
        }
      }
    })
  }

  pub fn to_string(&'a self) -> Cow<'a, str> {
    match self {
      Specifier::Relative(path) | Specifier::Absolute(path) | Specifier::Tilde(path) => {
        path.as_os_str().to_string_lossy()
      }
      Specifier::Hash(path) => path.clone(),
      Specifier::Package(module, subpath) => {
        if subpath.is_empty() {
          Cow::Borrowed(module)
        } else {
          Cow::Owned(format!("{}/{}", module, subpath))
        }
      }
      Specifier::Builtin(builtin) => Cow::Borrowed(&builtin),
      Specifier::Url(url) => Cow::Borrowed(url),
    }
  }
}

// https://url.spec.whatwg.org/#scheme-state
// https://github.com/servo/rust-url/blob/1c1e406874b3d2aa6f36c5d2f3a5c2ea74af9efb/url/src/parser.rs#L387
pub fn parse_scheme<'a>(input: &'a str) -> Result<(Cow<'a, str>, &'a str), ()> {
  if input.is_empty() || !input.starts_with(ascii_alpha) {
    return Err(());
  }
  let mut i = 0;
  let mut is_lowercase = true;
  for c in input.chars() {
    match c {
      'A'..='Z' => {
        is_lowercase = false;
      }
      'a'..='z' | '0'..='9' | '+' | '-' | '.' => {}
      ':' => {
        let scheme = &input[0..i];
        let rest = &input[i + 1..];
        return Ok(if is_lowercase {
          (Cow::Borrowed(scheme), rest)
        } else {
          (Cow::Owned(scheme.to_ascii_lowercase()), rest)
        });
      }
      _ => {
        return Err(());
      }
    }
    i += 1;
  }

  // EOF before ':'
  Err(())
}

// https://url.spec.whatwg.org/#path-state
fn parse_path<'a>(input: &'a str) -> (&'a str, &'a str) {
  // We don't really want to normalize the path (e.g. replacing ".." and "." segments).
  // That is done later. For now, we just need to find the end of the path.
  if let Some(pos) = input.chars().position(|c| c == '?' || c == '#') {
    (&input[0..pos], &input[pos..])
  } else {
    (input, "")
  }
}

// https://url.spec.whatwg.org/#query-state
fn parse_query<'a>(input: &'a str) -> (Option<&'a str>, &'a str) {
  if !input.is_empty() && input.as_bytes()[0] == b'?' {
    if let Some(pos) = input.chars().position(|c| c == '#') {
      (Some(&input[0..pos]), &input[pos..])
    } else {
      (Some(input), "")
    }
  } else {
    (None, input)
  }
}

/// https://url.spec.whatwg.org/#ascii-alpha
#[inline]
fn ascii_alpha(ch: char) -> bool {
  matches!(ch, 'a'..='z' | 'A'..='Z')
}

fn parse_package<'a>(specifier: Cow<'a, str>) -> Result<Specifier, SpecifierError> {
  match specifier {
    Cow::Borrowed(specifier) => {
      let (module, subpath) = parse_package_specifier(specifier)?;
      Ok(Specifier::Package(
        Cow::Borrowed(module),
        Cow::Borrowed(subpath),
      ))
    }
    Cow::Owned(specifier) => {
      let (module, subpath) = parse_package_specifier(&specifier)?;
      Ok(Specifier::Package(
        Cow::Owned(module.to_owned()),
        Cow::Owned(subpath.to_owned()),
      ))
    }
  }
}

pub fn parse_package_specifier(specifier: &str) -> Result<(&str, &str), SpecifierError> {
  let idx = specifier.chars().position(|p| p == '/');
  if specifier.starts_with('@') {
    let idx = idx.ok_or(SpecifierError::InvalidPackageSpecifier)?;
    if let Some(next) = &specifier[idx + 1..].chars().position(|p| p == '/') {
      Ok((
        &specifier[0..idx + 1 + *next],
        &specifier[idx + *next + 2..],
      ))
    } else {
      Ok((&specifier[..], ""))
    }
  } else if let Some(idx) = idx {
    Ok((&specifier[0..idx], &specifier[idx + 1..]))
  } else {
    Ok((&specifier[..], ""))
  }
}

pub fn decode_path<'a>(
  specifier: &'a str,
  specifier_type: SpecifierType,
) -> (Cow<'a, Path>, Option<&'a str>) {
  match specifier_type {
    SpecifierType::Url | SpecifierType::Esm => {
      let (path, rest) = parse_path(specifier);
      let (query, _) = parse_query(rest);
      let path = match percent_decode_str(path).decode_utf8_lossy() {
        Cow::Borrowed(v) => Cow::Borrowed(Path::new(v)),
        Cow::Owned(v) => Cow::Owned(PathBuf::from(v)),
      };
      (path, query)
    }
    SpecifierType::Cjs => (Cow::Borrowed(Path::new(specifier)), None),
  }
}

impl<'a> From<&'a str> for Specifier<'a> {
  fn from(specifier: &'a str) -> Self {
    Specifier::parse(specifier, SpecifierType::Cjs, Flags::empty())
      .unwrap()
      .0
  }
}

impl<'a, 'de: 'a> serde::Deserialize<'de> for Specifier<'a> {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    use serde::Deserialize;
    let s: &'de str = Deserialize::deserialize(deserializer)?;
    // Specifiers are only deserialized as part of the "alias" and "browser" fields,
    // so we assume CJS specifiers in Parcel mode.
    Specifier::parse(s, SpecifierType::Cjs, Flags::empty())
      .map(|s| s.0)
      .map_err(|_| serde::de::Error::custom("Invalid specifier"))
  }
}
