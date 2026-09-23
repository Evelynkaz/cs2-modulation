//! Versioned binary `.cgeo` collision mesh files.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use thiserror::Error;

use crate::mesh::{
    CollisionAttribute, CollisionMesh, MeshError, MeshObject, ObjectKind, SurfaceProperty,
};

const MAGIC: &[u8; 8] = b"CS2MGEO\0";
const FORMAT_VERSION: u32 = 1;

const TAG_VERT: u32 = u32::from_le_bytes(*b"VERT");
const TAG_TRIS: u32 = u32::from_le_bytes(*b"TRIS");
const TAG_TATR: u32 = u32::from_le_bytes(*b"TATR");
const TAG_TSRF: u32 = u32::from_le_bytes(*b"TSRF");
const TAG_TOBJ: u32 = u32::from_le_bytes(*b"TOBJ");
const TAG_ATTR: u32 = u32::from_le_bytes(*b"ATTR");
const TAG_SURF: u32 = u32::from_le_bytes(*b"SURF");
const TAG_OBJS: u32 = u32::from_le_bytes(*b"OBJS");
const TAG_DEGN: u32 = u32::from_le_bytes(*b"DEGN");

/// Minimum on-disk bytes per record, used to cap allocations from
/// file-declared counts before the bytes backing them have been checked.
const ATTR_MIN_RECORD: usize = 17; // 4 name-len + 3*4 list-count + 1 synthetic
const SURF_MIN_RECORD: usize = 5; // 4 hash + 1 opt-string tag
const OBJS_MIN_RECORD: usize = 10; // 1 kind + 4 opt-string tags + 4 source_index + 1 hull_flags tag

#[derive(Debug, Error)]
pub enum CgeoError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("not a .cgeo file (bad magic)")]
    BadMagic,
    #[error("unsupported .cgeo format version {0} (expected {FORMAT_VERSION})")]
    UnsupportedVersion(u32),
    #[error("CRC mismatch: payload is corrupt")]
    CrcMismatch,
    #[error("truncated .cgeo file")]
    Truncated,
    #[error("malformed .cgeo file: {0}")]
    Malformed(String),
    #[error("invalid string encoding: {0}")]
    InvalidString(#[from] std::string::FromUtf8Error),
    #[error("mesh failed validation: {0}")]
    Invalid(#[from] MeshError),
}

fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}
fn write_string_list(buf: &mut Vec<u8>, list: &[String]) {
    write_u32(buf, list.len() as u32);
    for s in list {
        write_string(buf, s);
    }
}
fn write_opt_string(buf: &mut Vec<u8>, s: &Option<String>) {
    match s {
        Some(s) => {
            buf.push(1);
            write_string(buf, s);
        }
        None => buf.push(0),
    }
}
fn write_section(out: &mut Vec<u8>, tag: u32, body: &[u8]) {
    write_u32(out, tag);
    write_u64(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn object_kind_tag(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::WorldHull => 0,
        ObjectKind::WorldMesh => 1,
        ObjectKind::WorldSphere => 2,
        ObjectKind::WorldCapsule => 3,
        ObjectKind::Entity => 4,
        ObjectKind::StaticProp => 5,
    }
}
fn object_kind_from_tag(tag: u8) -> Option<ObjectKind> {
    Some(match tag {
        0 => ObjectKind::WorldHull,
        1 => ObjectKind::WorldMesh,
        2 => ObjectKind::WorldSphere,
        3 => ObjectKind::WorldCapsule,
        4 => ObjectKind::Entity,
        5 => ObjectKind::StaticProp,
        _ => return None,
    })
}

/// Writes `mesh` and `meta` (opaque key/value metadata: map name, game build,
/// source hashes, extractor version, creation time) to `w` in `.cgeo` format.
/// Validates `mesh` first so a broken mesh cannot be written.
pub fn write_cgeo(
    mesh: &CollisionMesh,
    meta: &[(String, String)],
    mut w: impl Write,
) -> Result<(), CgeoError> {
    mesh.validate()?;

    let mut payload = Vec::new();

    // Metadata header.
    write_u32(&mut payload, meta.len() as u32);
    for (k, v) in meta {
        write_string(&mut payload, k);
        write_string(&mut payload, v);
    }

    // VERT
    let mut body = Vec::with_capacity(mesh.vertices.len() * 12);
    for v in &mesh.vertices {
        for c in v {
            body.extend_from_slice(&c.to_le_bytes());
        }
    }
    write_section(&mut payload, TAG_VERT, &body);

    // TRIS
    let mut body = Vec::with_capacity(mesh.triangles.len() * 12);
    for t in &mesh.triangles {
        for &i in t {
            write_u32(&mut body, i);
        }
    }
    write_section(&mut payload, TAG_TRIS, &body);

    // TATR
    let mut body = Vec::with_capacity(mesh.tri_attribute.len() * 2);
    for &a in &mesh.tri_attribute {
        body.extend_from_slice(&a.to_le_bytes());
    }
    write_section(&mut payload, TAG_TATR, &body);

    // TSRF
    let mut body = Vec::with_capacity(mesh.tri_surface.len() * 2);
    for &s in &mesh.tri_surface {
        body.extend_from_slice(&s.to_le_bytes());
    }
    write_section(&mut payload, TAG_TSRF, &body);

    // TOBJ
    let mut body = Vec::with_capacity(mesh.tri_object.len() * 4);
    for &o in &mesh.tri_object {
        write_u32(&mut body, o);
    }
    write_section(&mut payload, TAG_TOBJ, &body);

    // ATTR
    let mut body = Vec::new();
    write_u32(&mut body, mesh.attributes.len() as u32);
    for a in &mesh.attributes {
        write_string(&mut body, &a.name);
        write_string_list(&mut body, &a.interact_as);
        write_string_list(&mut body, &a.interact_with);
        write_string_list(&mut body, &a.interact_exclude);
        body.push(a.synthetic as u8);
    }
    write_section(&mut payload, TAG_ATTR, &body);

    // SURF
    let mut body = Vec::new();
    write_u32(&mut body, mesh.surfaces.len() as u32);
    for s in &mesh.surfaces {
        write_u32(&mut body, s.hash);
        write_opt_string(&mut body, &s.name);
    }
    write_section(&mut payload, TAG_SURF, &body);

    // OBJS
    let mut body = Vec::new();
    write_u32(&mut body, mesh.objects.len() as u32);
    for o in &mesh.objects {
        body.push(object_kind_tag(o.kind));
        write_opt_string(&mut body, &o.classname);
        write_opt_string(&mut body, &o.targetname);
        write_opt_string(&mut body, &o.model);
        write_opt_string(&mut body, &o.hammer_id);
        write_u32(&mut body, o.source_index);
        match o.hull_flags {
            Some(f) => {
                body.push(1);
                write_u32(&mut body, f);
            }
            None => body.push(0),
        }
    }
    write_section(&mut payload, TAG_OBJS, &body);

    // DEGN
    let mut body = Vec::with_capacity(8);
    write_u64(&mut body, mesh.degenerate_skipped);
    write_section(&mut payload, TAG_DEGN, &body);

    let crc = crc32fast::hash(&payload);

    w.write_all(MAGIC)?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // flags
    w.write_all(&(payload.len() as u64).to_le_bytes())?;
    w.write_all(&crc.to_le_bytes())?;
    w.write_all(&payload)?;
    Ok(())
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], CgeoError> {
        if self.pos + n > self.data.len() {
            return Err(CgeoError::Truncated);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, CgeoError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, CgeoError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, CgeoError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, CgeoError> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, CgeoError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        Ok(String::from_utf8(bytes.to_vec())?)
    }
    fn string_list(&mut self) -> Result<Vec<String>, CgeoError> {
        let n = self.u32()? as usize;
        // Each string is at least a 4-byte length prefix.
        let mut out = Vec::with_capacity(n.min(self.remaining() / 4));
        for _ in 0..n {
            out.push(self.string()?);
        }
        Ok(out)
    }
    fn opt_string(&mut self) -> Result<Option<String>, CgeoError> {
        Ok(if self.u8()? != 0 {
            Some(self.string()?)
        } else {
            None
        })
    }
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

/// Reads a `.cgeo` file, returning the mesh and its opaque metadata list.
/// Validates magic, version, flags, CRC, section order/lengths, rejects
/// trailing bytes, then runs `mesh.validate()`. An `io::Error` other than
/// unexpected EOF is propagated as-is; unexpected EOF becomes `Truncated`.
pub fn read_cgeo(mut r: impl Read) -> Result<(CollisionMesh, Vec<(String, String)>), CgeoError> {
    let mut header = [0u8; 8 + 4 + 4 + 8 + 4];
    read_exact_mapped(&mut r, &mut header)?;
    if header[0..8] != *MAGIC {
        return Err(CgeoError::BadMagic);
    }
    let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(CgeoError::UnsupportedVersion(version));
    }
    let flags = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if flags != 0 {
        return Err(CgeoError::Malformed(format!(
            "unsupported flags {flags:#010x}"
        )));
    }
    let payload_len = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let expected_crc = u32::from_le_bytes(header[24..28].try_into().unwrap());

    const MAX_REASONABLE: u64 = 4 * 1024 * 1024 * 1024;
    if payload_len > MAX_REASONABLE {
        return Err(CgeoError::Malformed(format!(
            "payload length {payload_len} exceeds sanity cap"
        )));
    }

    let mut payload = Vec::new();
    let mut limited = r.take(payload_len);
    limited.read_to_end(&mut payload).map_err(map_io_err)?;
    if payload.len() as u64 != payload_len {
        return Err(CgeoError::Truncated);
    }
    let mut r = limited.into_inner();

    // Detect bytes after the declared payload: reading one more byte should
    // hit EOF (Ok(0)); anything else means there is trailing data.
    let mut extra = [0u8; 1];
    match r.read(&mut extra) {
        Ok(0) => {}
        Ok(_) => {
            return Err(CgeoError::Malformed(
                "trailing bytes after payload".to_string(),
            ));
        }
        Err(e) => return Err(map_io_err(e)),
    }

    let actual_crc = crc32fast::hash(&payload);
    if actual_crc != expected_crc {
        return Err(CgeoError::CrcMismatch);
    }

    let mut c = Cursor::new(&payload);

    let meta_count = c.u32()? as usize;
    // Each metadata entry is at least two 4-byte length prefixes.
    let mut meta = Vec::with_capacity(meta_count.min(c.remaining() / 8).min(1 << 16));
    for _ in 0..meta_count {
        let k = c.string()?;
        let v = c.string()?;
        meta.push((k, v));
    }

    let expected_order = [
        ("VERT", TAG_VERT),
        ("TRIS", TAG_TRIS),
        ("TATR", TAG_TATR),
        ("TSRF", TAG_TSRF),
        ("TOBJ", TAG_TOBJ),
        ("ATTR", TAG_ATTR),
        ("SURF", TAG_SURF),
        ("OBJS", TAG_OBJS),
        ("DEGN", TAG_DEGN),
    ];

    let mut mesh = CollisionMesh::new();
    let mut vertices: Vec<[f32; 3]> = Vec::new();
    let mut triangles: Vec<[u32; 3]> = Vec::new();

    for (name, expected_tag) in expected_order {
        let tag = c.u32()?;
        if tag != expected_tag {
            return Err(CgeoError::Malformed(format!(
                "expected section {name}, found tag {tag:#010x}"
            )));
        }
        let len = c.u64()? as usize;
        if len > c.remaining() {
            return Err(CgeoError::Malformed(format!(
                "{name} section length exceeds remaining payload"
            )));
        }
        let section_end = c.pos + len;
        match expected_tag {
            TAG_VERT => {
                if !len.is_multiple_of(12) {
                    return Err(CgeoError::Malformed(format!(
                        "{name} length not a multiple of 12"
                    )));
                }
                let n = len / 12;
                vertices.reserve(n);
                for _ in 0..n {
                    vertices.push([c.f32()?, c.f32()?, c.f32()?]);
                }
            }
            TAG_TRIS => {
                if !len.is_multiple_of(12) {
                    return Err(CgeoError::Malformed(format!(
                        "{name} length not a multiple of 12"
                    )));
                }
                let n = len / 12;
                triangles.reserve(n);
                for _ in 0..n {
                    triangles.push([c.u32()?, c.u32()?, c.u32()?]);
                }
            }
            TAG_TATR => {
                if !len.is_multiple_of(2) {
                    return Err(CgeoError::Malformed(format!(
                        "{name} length not a multiple of 2"
                    )));
                }
                for _ in 0..len / 2 {
                    mesh.tri_attribute
                        .push(u16::from_le_bytes(c.take(2)?.try_into().unwrap()));
                }
            }
            TAG_TSRF => {
                if !len.is_multiple_of(2) {
                    return Err(CgeoError::Malformed(format!(
                        "{name} length not a multiple of 2"
                    )));
                }
                for _ in 0..len / 2 {
                    mesh.tri_surface
                        .push(u16::from_le_bytes(c.take(2)?.try_into().unwrap()));
                }
            }
            TAG_TOBJ => {
                if !len.is_multiple_of(4) {
                    return Err(CgeoError::Malformed(format!(
                        "{name} length not a multiple of 4"
                    )));
                }
                for _ in 0..len / 4 {
                    mesh.tri_object.push(c.u32()?);
                }
            }
            TAG_ATTR => {
                let n = c.u32()? as usize;
                let cap = len.saturating_sub(4) / ATTR_MIN_RECORD;
                mesh.attributes.reserve(n.min(cap));
                for _ in 0..n {
                    let name = c.string()?;
                    let interact_as = c.string_list()?;
                    let interact_with = c.string_list()?;
                    let interact_exclude = c.string_list()?;
                    let synthetic = c.u8()? != 0;
                    mesh.attributes.push(CollisionAttribute {
                        name,
                        interact_as,
                        interact_with,
                        interact_exclude,
                        synthetic,
                    });
                }
            }
            TAG_SURF => {
                let n = c.u32()? as usize;
                let cap = len.saturating_sub(4) / SURF_MIN_RECORD;
                mesh.surfaces.reserve(n.min(cap));
                for _ in 0..n {
                    let hash = c.u32()?;
                    let name = c.opt_string()?;
                    mesh.surfaces.push(SurfaceProperty { hash, name });
                }
            }
            TAG_OBJS => {
                let n = c.u32()? as usize;
                let cap = len.saturating_sub(4) / OBJS_MIN_RECORD;
                mesh.objects.reserve(n.min(cap));
                for _ in 0..n {
                    let kind_tag = c.u8()?;
                    let kind = object_kind_from_tag(kind_tag).ok_or_else(|| {
                        CgeoError::Malformed(format!("unknown object kind tag {kind_tag}"))
                    })?;
                    let classname = c.opt_string()?;
                    let targetname = c.opt_string()?;
                    let model = c.opt_string()?;
                    let hammer_id = c.opt_string()?;
                    let source_index = c.u32()?;
                    let hull_flags = if c.u8()? != 0 { Some(c.u32()?) } else { None };
                    mesh.objects.push(MeshObject {
                        kind,
                        classname,
                        targetname,
                        model,
                        hammer_id,
                        source_index,
                        hull_flags,
                    });
                }
            }
            TAG_DEGN => {
                if len != 8 {
                    return Err(CgeoError::Malformed(format!(
                        "{name} length must be 8, got {len}"
                    )));
                }
                mesh.degenerate_skipped = c.u64()?;
            }
            _ => unreachable!(),
        }
        if c.pos != section_end {
            return Err(CgeoError::Malformed(format!(
                "{name} declared length did not match contents"
            )));
        }
    }

    if c.pos != payload.len() {
        return Err(CgeoError::Malformed(
            "trailing bytes after last section".to_string(),
        ));
    }

    mesh.vertices = vertices;
    mesh.triangles = triangles;
    mesh.validate()?;

    Ok((mesh, meta))
}

fn map_io_err(e: io::Error) -> CgeoError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        CgeoError::Truncated
    } else {
        CgeoError::Io(e)
    }
}

fn read_exact_mapped(r: &mut impl Read, buf: &mut [u8]) -> Result<(), CgeoError> {
    r.read_exact(buf).map_err(map_io_err)
}

/// Writes `mesh` atomically to `path`: validates `mesh`, writes to
/// `<path>.tmp`, `fsync`s it, then renames it into place. On any failure
/// (including a rename failure) the temp file is removed before the error is
/// returned.
pub fn save_cgeo(
    path: impl AsRef<Path>,
    mesh: &CollisionMesh,
    meta: &[(String, String)],
) -> Result<(), CgeoError> {
    mesh.validate()?;

    let path = path.as_ref();
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = Path::new(&tmp_name);

    let write_result: Result<(), CgeoError> = (|| {
        let mut file = fs::File::create(tmp_path)?;
        write_cgeo(mesh, meta, &mut file)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(tmp_path);
        return Err(e);
    }

    if let Err(e) = fs::rename(tmp_path, path) {
        let _ = fs::remove_file(tmp_path);
        return Err(e.into());
    }
    Ok(())
}

/// Loads a `.cgeo` file from `path`.
pub fn load_cgeo(
    path: impl AsRef<Path>,
) -> Result<(CollisionMesh, Vec<(String, String)>), CgeoError> {
    let data = fs::read(path)?;
    read_cgeo(&data[..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::SurfaceProperty as Surf;

    fn sample_mesh() -> CollisionMesh {
        let mut mesh = CollisionMesh::new();
        let a = mesh
            .add_attribute(CollisionAttribute {
                name: "Default".to_string(),
                interact_as: vec!["passbullets".to_string()],
                interact_with: vec![],
                interact_exclude: vec![],
                synthetic: false,
            })
            .unwrap();
        let s = mesh
            .add_surface(Surf {
                hash: 12345,
                name: Some("concrete".to_string()),
            })
            .unwrap();
        let o = mesh.add_object(MeshObject {
            kind: ObjectKind::WorldHull,
            classname: Some("worldspawn".to_string()),
            targetname: None,
            model: None,
            hammer_id: Some("42".to_string()),
            source_index: 0,
            hull_flags: Some(2),
        });
        mesh.push_triangles(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| s,
            o,
        )
        .unwrap();
        // One degenerate triangle so degenerate_skipped round-trips non-zero.
        mesh.push_triangles(
            &[[5.0, 0.0, 0.0], [6.0, 0.0, 0.0], [7.0, 0.0, 0.0]],
            &[[0, 1, 2]],
            a,
            |_| SurfaceProperty::NONE,
            o,
        )
        .unwrap();
        mesh
    }

    #[test]
    fn round_trip_equal() {
        let mesh = sample_mesh();
        let meta = vec![("map".to_string(), "de_mirage".to_string())];
        let mut buf = Vec::new();
        write_cgeo(&mesh, &meta, &mut buf).unwrap();
        let (mesh2, meta2) = read_cgeo(&buf[..]).unwrap();
        assert_eq!(mesh.vertices, mesh2.vertices);
        assert_eq!(mesh.triangles, mesh2.triangles);
        assert_eq!(mesh.tri_attribute, mesh2.tri_attribute);
        assert_eq!(mesh.tri_surface, mesh2.tri_surface);
        assert_eq!(mesh.tri_object, mesh2.tri_object);
        assert_eq!(mesh.attributes, mesh2.attributes);
        assert_eq!(mesh.surfaces, mesh2.surfaces);
        assert_eq!(mesh.objects, mesh2.objects);
        assert_eq!(mesh.degenerate_skipped, mesh2.degenerate_skipped);
        assert_eq!(mesh.degenerate_skipped, 1);
        assert_eq!(meta, meta2);
    }

    #[test]
    fn crc_corruption_detected() {
        let mesh = sample_mesh();
        let mut buf = Vec::new();
        write_cgeo(&mesh, &[], &mut buf).unwrap();
        let last = buf.len() - 1;
        buf[last] ^= 0xFF;
        let err = read_cgeo(&buf[..]).unwrap_err();
        assert!(matches!(err, CgeoError::CrcMismatch));
    }

    #[test]
    fn truncated_file_errors() {
        let mesh = sample_mesh();
        let mut buf = Vec::new();
        write_cgeo(&mesh, &[], &mut buf).unwrap();
        buf.truncate(buf.len() / 2);
        let err = read_cgeo(&buf[..]).unwrap_err();
        assert!(matches!(err, CgeoError::Truncated));
    }

    #[test]
    fn wrong_magic_errors() {
        let mesh = sample_mesh();
        let mut buf = Vec::new();
        write_cgeo(&mesh, &[], &mut buf).unwrap();
        buf[0] = b'X';
        let err = read_cgeo(&buf[..]).unwrap_err();
        assert!(matches!(err, CgeoError::BadMagic));
    }

    #[test]
    fn wrong_version_errors() {
        let mesh = sample_mesh();
        let mut buf = Vec::new();
        write_cgeo(&mesh, &[], &mut buf).unwrap();
        buf[8..12].copy_from_slice(&99u32.to_le_bytes());
        let err = read_cgeo(&buf[..]).unwrap_err();
        assert!(matches!(err, CgeoError::UnsupportedVersion(99)));
    }

    #[test]
    fn nonzero_flags_rejected() {
        let mesh = sample_mesh();
        let mut buf = Vec::new();
        write_cgeo(&mesh, &[], &mut buf).unwrap();
        buf[12..16].copy_from_slice(&1u32.to_le_bytes());
        let err = read_cgeo(&buf[..]).unwrap_err();
        assert!(matches!(err, CgeoError::Malformed(_)));
    }

    #[test]
    fn trailing_bytes_rejected() {
        let mesh = sample_mesh();
        let mut buf = Vec::new();
        write_cgeo(&mesh, &[], &mut buf).unwrap();
        // Bump the declared payload length and append a stray byte so CRC
        // still needs to be recomputed over the (now longer) payload.
        let payload_start = 28usize;
        let mut payload = buf[payload_start..].to_vec();
        payload.push(0xAB);
        let new_len = payload.len() as u64;
        let new_crc = crc32fast::hash(&payload);
        let mut new_buf = Vec::new();
        new_buf.extend_from_slice(&buf[0..8]);
        new_buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        new_buf.extend_from_slice(&0u32.to_le_bytes());
        new_buf.extend_from_slice(&new_len.to_le_bytes());
        new_buf.extend_from_slice(&new_crc.to_le_bytes());
        new_buf.extend_from_slice(&payload);
        let err = read_cgeo(&new_buf[..]).unwrap_err();
        assert!(matches!(err, CgeoError::Malformed(_)));
    }

    #[test]
    fn attr_count_overflow_does_not_allocate_hugely() {
        let mut payload = Vec::new();
        write_u32(&mut payload, 0); // meta count
        write_section(&mut payload, TAG_VERT, &[]);
        write_section(&mut payload, TAG_TRIS, &[]);
        write_section(&mut payload, TAG_TATR, &[]);
        write_section(&mut payload, TAG_TSRF, &[]);
        write_section(&mut payload, TAG_TOBJ, &[]);
        let mut attr_body = Vec::new();
        write_u32(&mut attr_body, u32::MAX);
        write_section(&mut payload, TAG_ATTR, &attr_body);

        let crc = crc32fast::hash(&payload);
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&payload);

        let err = read_cgeo(&buf[..]).unwrap_err();
        assert!(matches!(
            err,
            CgeoError::Truncated | CgeoError::Malformed(_)
        ));
    }

    #[test]
    fn trailing_bytes_after_payload_rejected() {
        let mesh = sample_mesh();
        let mut buf = Vec::new();
        write_cgeo(&mesh, &[], &mut buf).unwrap();
        buf.push(0xEE); // stray byte after the declared payload, not counted in its length or CRC
        let err = read_cgeo(&buf[..]).unwrap_err();
        assert!(matches!(err, CgeoError::Malformed(_)));
    }

    /// A `std::env::temp_dir()` subdirectory unique to one test, removed (recursively) on drop,
    /// even on panic.
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn save_invalid_mesh_leaves_no_tmp() {
        let mut mesh = CollisionMesh::new();
        mesh.vertices = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        mesh.triangles = vec![[0, 1, 2]];
        // tri_attribute/tri_surface/tri_object left empty -> length mismatch.
        let dir = TempDir(
            std::env::temp_dir().join(format!("geom_cgeo_invalid_test_{}", std::process::id())),
        );
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("invalid.cgeo");
        let err = save_cgeo(&path, &mesh, &[]).unwrap_err();
        assert!(matches!(err, CgeoError::Invalid(_)));
        assert!(!path.exists());
        assert!(!path.with_extension("cgeo.tmp").exists());
    }

    #[test]
    fn save_and_load_round_trip() {
        let mesh = sample_mesh();
        let dir =
            TempDir(std::env::temp_dir().join(format!("geom_cgeo_test_{}", std::process::id())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("test.cgeo");
        save_cgeo(&path, &mesh, &[]).unwrap();
        let (mesh2, _) = load_cgeo(&path).unwrap();
        assert_eq!(mesh.vertices, mesh2.vertices);
        assert_eq!(mesh.degenerate_skipped, mesh2.degenerate_skipped);
        assert!(!path.with_extension("cgeo.tmp").exists());
    }
}
