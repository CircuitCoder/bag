use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::{self, Write as _},
    string::FromUtf8Error,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Segment<'a>(pub Cow<'a, str>, pub BTreeMap<Cow<'a, str>, Cow<'a, str>>);

impl<'a> Segment<'a> {
    pub const fn new(name: Cow<'a, str>, args: BTreeMap<Cow<'a, str>, Cow<'a, str>>) -> Self {
        Segment(name, args)
    }
}

impl fmt::Display for Segment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", urlencoding::encode(self.0.as_ref()))?;
        for (k, v) in self.1.iter() {
            write!(f, ",{}={}", urlencoding::encode(k), urlencoding::encode(v))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SegmentParseError {
    InvalidArgument(String),
    DecodeError(FromUtf8Error),
}

impl<'s> TryFrom<&'s str> for Segment<'s> {
    type Error = SegmentParseError;

    fn try_from(s: &'s str) -> Result<Self, Self::Error> {
        let split = s.split_once(",");
        let first = split.as_ref().map(|(first, _)| *first).unwrap_or(s);
        let segment = urlencoding::decode(first).map_err(SegmentParseError::DecodeError)?;

        let collected = if let Some((_, next)) = split {
            let collected: Result<BTreeMap<Cow<'_, str>, Cow<'_, str>>, Self::Error> = next
                .split(",")
                .map(|arg| {
                    let equal_cnt = arg.matches("=").count();
                    if equal_cnt != 1 {
                        return Err(SegmentParseError::InvalidArgument(arg.to_string()));
                    }
                    let (k_raw, v_raw) = arg.split_once("=").unwrap();
                    let k = urlencoding::decode(k_raw).map_err(SegmentParseError::DecodeError)?;
                    let v = urlencoding::decode(v_raw).map_err(SegmentParseError::DecodeError)?;
                    Ok((k, v))
                })
                .collect();
            collected?
        } else {
            BTreeMap::new()
        };

        Ok(Segment(segment, collected))
    }
}

impl<'s> Segment<'s> {
    pub fn name(&self) -> &str {
        self.0.as_ref()
    }

    pub fn arg(&self, key: &str) -> Option<&str> {
        self.1.get(key).map(|v| v.as_ref())
    }

    pub fn with_arg<'r>(&'r self, key: &'r str, value: Option<&'r str>) -> Segment<'r>
    where
        's: 'r,
    {
        let mut new_args = self.1.clone();
        if let Some(value) = value {
            new_args.insert(Cow::Borrowed(key), Cow::Borrowed(value));
        } else {
            new_args.remove(key);
        }
        Segment(self.0.clone(), new_args)
    }
}

#[cfg(test)]
mod tests {
    use super::{Path, Segment};

    #[test]
    fn segment_arguments_have_a_canonical_serialization_order() {
        let first = Segment::try_from("photo,offset=40,limit=20,password=secret").unwrap();
        let second = Segment::try_from("photo,password=secret,limit=20,offset=40").unwrap();

        assert_eq!(
            first.to_string(),
            "photo,limit=20,offset=40,password=secret"
        );
        assert_eq!(first.to_string(), second.to_string());
    }

    #[test]
    fn path_updates_preserve_canonical_argument_order() {
        let path = Path::try_from("file/archive.zip/%3A,password=secret,offset=40").unwrap();
        let updated = path.with_arg("limit", Some("20"));

        assert_eq!(
            updated.to_string(),
            "file/archive.zip/%3A,limit=20,offset=40,password=secret"
        );
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Path<'a>(pub Cow<'a, [Segment<'a>]>);

impl<'s> AsRef<[Segment<'s>]> for Path<'s> {
    fn as_ref(&self) -> &[Segment<'s>] {
        self.0.as_ref()
    }
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, segment) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_char('/')?;
            }
            write!(f, "{segment}")?;
        }
        Ok(())
    }
}

impl<'s> TryFrom<&'s str> for Path<'s> {
    type Error = SegmentParseError;

    fn try_from(s: &'s str) -> Result<Self, Self::Error> {
        if s.is_empty() {
            return Ok(Path(Cow::Borrowed(&[])));
        }

        let segments: Result<Vec<Segment>, Self::Error> =
            s.split("/").map(Segment::try_from).collect();
        // There is at least one
        Ok(Path(Cow::Owned(segments?)))
    }
}

impl<'s> Path<'s> {
    pub const fn empty() -> Self {
        Path(Cow::Borrowed(&[]))
    }

    pub const fn new(inner: Cow<'s, [Segment<'s>]>) -> Self {
        Path(inner)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn parent(&self) -> Option<Path<'_>> {
        if self.0.len() <= 1 {
            return None;
        }

        let parent_segments = &self.0[..self.0.len() - 1];
        Some(Path(Cow::Borrowed(parent_segments)))
    }

    pub fn segments(&self) -> &[Segment<'s>] {
        self.0.as_ref()
    }

    pub fn last(&self) -> Option<&Segment<'s>> {
        self.0.last()
    }

    pub fn first(&self) -> Option<&Segment<'s>> {
        self.0.first()
    }

    pub fn next(&self) -> Option<Path<'_>> {
        if self.0.len() <= 1 {
            return None;
        }

        let next_segments = &self.0[1..];
        Some(Path(Cow::Borrowed(next_segments)))
    }

    pub fn with_arg<'r>(&'r self, key: &'r str, value: Option<&'r str>) -> Path<'r>
    where
        's: 'r,
    {
        if self.0.is_empty() {
            return Path(Cow::Borrowed(&[]));
        }

        let mut modified = Vec::from(self.0.as_ref());
        let modified_last = self.0[self.0.len() - 1].with_arg(key, value);
        modified[self.0.len() - 1] = modified_last;
        Path(Cow::Owned(modified))
    }

    pub fn to_bare_string(&self) -> String {
        itertools::join(self.0.iter().map(|s| s.0.as_ref()), "/")
    }

    pub fn split_subpath(&self, marker: &str) -> (Path<'_>, Option<Path<'_>>) {
        for (index, segment) in self.0.iter().enumerate() {
            if segment.name() == marker {
                let first = &self.0[..index];
                let second = &self.0[index..];
                return (
                    Path(Cow::Borrowed(first)),
                    Some(Path(Cow::Borrowed(second))),
                );
            }
        }

        (Path(Cow::Borrowed(&self.0)), None)
    }

    pub fn to_static(&self) -> Path<'static> {
        let static_segments: Vec<Segment<'static>> = self
            .0
            .iter()
            .map(|s| {
                Segment(
                    Cow::Owned(s.0.as_ref().to_owned()),
                    s.1.iter()
                        .map(|(k, v)| {
                            (
                                Cow::Owned(k.as_ref().to_owned()),
                                Cow::Owned(v.as_ref().to_owned()),
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        Path(Cow::Owned(static_segments))
    }

    pub fn prefix(&self, to: usize) -> Option<Path<'_>> {
        if to > self.0.len() {
            return None;
        }

        let prefix_segments = &self.0[..to.min(self.0.len())];
        Some(Path(Cow::Borrowed(prefix_segments)))
    }

    pub fn suffix(&self, from: usize) -> Option<Path<'_>> {
        if from == self.0.len() {
            return Some(Path(Cow::Borrowed(&[])));
        }

        if from > self.0.len() {
            return None;
        }

        let suffix_segments = &self.0[from..];
        Some(Path(Cow::Borrowed(suffix_segments)))
    }

    pub fn borrow(&self) -> Path<'_> {
        Path(Cow::Borrowed(self.0.as_ref()))
    }
}
