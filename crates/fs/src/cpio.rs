//! CPIO "newc" (SVR4 without CRC) archive parser.

use crate::ramfs::{ramfs_create_child, ramfs_write_file};
use crate::vnode::{VNode, VNodeType};

pub const CPIO_MAGIC: &[u8; 6] = b"070701";
pub const CPIO_HEADER_SIZE: usize = 110;

/// Parse one 8-character uppercase hex ASCII field.
pub fn parse_hex8(s: &[u8; 8]) -> u32 {
    let mut v = 0u32;
    for &b in s {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'A'..=b'F' => b - b'A' + 10,
            b'a'..=b'f' => b - b'a' + 10,
            _ => return 0,
        };
        v = v.wrapping_shl(4) | d as u32;
    }
    v
}

#[inline]
pub fn align4(x: usize) -> usize {
    (x + 3) & !3
}

struct Entry {
    mode: u32,
    filesize: usize,
    namesize: usize,
}

fn parse_header(d: &[u8]) -> Option<Entry> {
    if d.len() < CPIO_HEADER_SIZE {
        return None;
    }
    if &d[..6] != CPIO_MAGIC {
        return None;
    }
    // Field layout (all 8-char hex ASCII):
    //   magic[0..6], ino[6..14], mode[14..22], uid[22..30], gid[30..38],
    //   nlink[38..46], mtime[46..54], filesize[54..62], devmaj[62..70],
    //   devmin[70..78], rdevmaj[78..86], rdevmin[86..94],
    //   namesize[94..102], check[102..110]
    let mode = parse_hex8(d[14..22].try_into().ok()?);
    let filesize = parse_hex8(d[54..62].try_into().ok()?) as usize;
    let namesize = parse_hex8(d[94..102].try_into().ok()?) as usize;
    Some(Entry {
        mode,
        filesize,
        namesize,
    })
}

/// Unpack a CPIO newc archive into the ramfs directory `root`.
///
/// # Safety
/// `data` must be a valid CPIO newc archive.
/// `root` must be a live ramfs directory VNode.
pub unsafe fn unpack_cpio(data: &[u8], root: *mut VNode) {
    let mut pos = 0usize;
    while let Some(e) = parse_header(&data[pos..]) {
        pos += CPIO_HEADER_SIZE;

        if pos + e.namesize > data.len() {
            break;
        }
        let name_bytes = &data[pos..pos + e.namesize.saturating_sub(1)];
        pos += e.namesize;
        pos = align4(pos);

        let data_end = pos + e.filesize;
        if data_end > data.len() {
            break;
        }
        let file_data = &data[pos..data_end];
        pos = align4(data_end);

        if name_bytes == b"TRAILER!!!" {
            break;
        }

        let name_str = match core::str::from_utf8(name_bytes) {
            Ok(s) => s.trim_start_matches("./"),
            Err(_) => continue,
        };
        if name_str.is_empty() {
            continue;
        }

        match e.mode & 0o170000 {
            0o100000 => {
                // Regular file
                let vn = ensure_path(root, name_str);
                if !vn.is_null() {
                    ramfs_write_file(vn, file_data);
                }
            }
            0o040000 => {
                ensure_dir(root, name_str);
            }
            _ => {}
        }
    }
}

fn ensure_path(root: *mut VNode, path: &str) -> *mut VNode {
    let mut dir = root;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty() {
            continue;
        }
        if parts.peek().is_none() {
            return match lookup_or_create(dir, part, VNodeType::Regular) {
                Some(v) => v,
                None => core::ptr::null_mut(),
            };
        }
        dir = match lookup_or_create(dir, part, VNodeType::Directory) {
            Some(v) => v,
            None => return core::ptr::null_mut(),
        };
    }
    core::ptr::null_mut()
}

fn ensure_dir(root: *mut VNode, path: &str) -> *mut VNode {
    let mut cur = root;
    for part in path.split('/') {
        if part.is_empty() {
            continue;
        }
        cur = match lookup_or_create(cur, part, VNodeType::Directory) {
            Some(v) => v,
            None => return core::ptr::null_mut(),
        };
    }
    cur
}

fn lookup_or_create(dir: *mut VNode, name: &str, vtype: VNodeType) -> Option<*mut VNode> {
    if dir.is_null() {
        return None;
    }
    let vn = unsafe { &*dir };
    if let Some(ex) = (vn.ops.lookup)(vn, name) {
        return Some(ex);
    }
    ramfs_create_child(dir, name, vtype)
}

// ─── tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex8_basic() {
        assert_eq!(parse_hex8(b"0000002A"), 42);
    }
    #[test]
    fn parse_hex8_zero() {
        assert_eq!(parse_hex8(b"00000000"), 0);
    }
    #[test]
    fn parse_hex8_max() {
        assert_eq!(parse_hex8(b"FFFFFFFF"), 0xFFFF_FFFF);
    }
    #[test]
    fn parse_hex8_lower() {
        assert_eq!(parse_hex8(b"0000001a"), 26);
    }
    #[test]
    fn align4_zero() {
        assert_eq!(align4(0), 0);
    }
    #[test]
    fn align4_one() {
        assert_eq!(align4(1), 4);
    }
    #[test]
    fn align4_four() {
        assert_eq!(align4(4), 4);
    }
    #[test]
    fn align4_five() {
        assert_eq!(align4(5), 8);
    }
    #[test]
    fn cpio_magic() {
        assert_eq!(CPIO_MAGIC, b"070701");
    }
}
