use std::{borrow::Cow, collections::HashMap, string::FromUtf8Error};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment<'a>(pub Cow<'a, str>, pub HashMap<Cow<'a, str>, Cow<'a, str>>);

impl<'a> Segment<'a> {
    pub const fn new(name: Cow<'a, str>, args: HashMap<Cow<'a, str>, Cow<'a, str>>) -> Self {
        Segment(name, args)
    }
}

impl ToString for Segment<'_> {
    fn to_string(&self) -> String {
        let mut result = urlencoding::encode(self.0.as_ref()).to_string();

        for (k, v) in self.1.iter() {
            result.push(',');
            result.push_str(&urlencoding::encode(k));
            result.push('=');
            result.push_str(&urlencoding::encode(v));
        }

        result
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
            let collected: Result<HashMap<Cow<'_, str>, Cow<'_, str>>, Self::Error> = next
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
            HashMap::new()
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path<'a>(pub Cow<'a, [Segment<'a>]>);

impl ToString for Path<'_> {
    fn to_string(&self) -> String {
        itertools::join(self.0.iter().map(|s| s.to_string()), "/")
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
    pub const fn new(inner: Cow<'s, [Segment<'s>]>) -> Self {
        Path(inner)
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
}
