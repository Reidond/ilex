#[cfg(target_os = "windows")]
pub fn image() -> (Vec<u8>, Vec<u8>) {
    // Microsoft x64 ABI: RDX is EFI_SYSTEM_TABLE*. Call the firmware's
    // ConOut->OutputString with its own interface and a UTF-16 marker.
    let mut code = vec![
        0x48, 0x8b, 0x42, 0x40, // mov rax, [rdx+0x40] (ConOut)
        0x48, 0x89, 0xc1, // mov rcx, rax
        0x48, 0x8d, 0x15, 13, 0, 0, 0, // lea rdx, [rip+13]
        0x48, 0x83, 0xec, 0x28, // sub rsp, 0x28 (ABI shadow space and alignment)
        0xff, 0x50, 8, // call [rax+8] (OutputString)
        0x48, 0x83, 0xc4, 0x28, // add rsp, 0x28
        0xeb, 0xfe, // jmp $
    ];
    code.extend(
        "ILEX UNSIGNED EXECUTED\r\n\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    let payload = unsigned_efi(&code);
    assert!(
        payload[0x98 + 112 + 4 * 8..0x98 + 112 + 5 * 8]
            .iter()
            .all(|&b| b == 0)
    );
    let mut disk = fat_disk(&payload);
    add_gpt(&mut disk);
    (payload, disk)
}

/// Gen2 discovers its fallback boot file from a GPT ESP.
#[cfg(target_os = "windows")]
fn add_gpt(disk: &mut Vec<u8>) {
    const SECTOR: usize = 512;
    const ESP: u64 = 2048;
    const TABLE_SECTORS: u64 = 32;
    let esp_end = (disk.len() / SECTOR - 1) as u64;
    let last = esp_end + TABLE_SECTORS + 1;
    disk.resize((last as usize + 1) * SECTOR, 0);
    disk[450] = 0xee;
    disk[454..458].copy_from_slice(&1u32.to_le_bytes());
    disk[458..462].copy_from_slice(&(last as u32).to_le_bytes());

    let mut entries = vec![0; TABLE_SECTORS as usize * SECTOR];
    // EFI System Partition type GUID, then a fixture-specific partition GUID.
    entries[..16].copy_from_slice(&[
        0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9,
        0x3b,
    ]);
    entries[16..32].copy_from_slice(b"ILEX-GEN2-EFI-01");
    entries[32..40].copy_from_slice(&ESP.to_le_bytes());
    entries[40..48].copy_from_slice(&esp_end.to_le_bytes());
    let crc = crc32fast::hash(&entries);
    for (here, other, table) in [(1u64, last, 2u64), (last, 1, esp_end + 1)] {
        let entries_start = table as usize * SECTOR;
        disk[entries_start..entries_start + entries.len()].copy_from_slice(&entries);
        let offset = here as usize * SECTOR;
        let header = &mut disk[offset..offset + SECTOR];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&here.to_le_bytes());
        header[32..40].copy_from_slice(&other.to_le_bytes());
        header[40..48].copy_from_slice(&34u64.to_le_bytes());
        header[48..56].copy_from_slice(&esp_end.to_le_bytes());
        header[56..72].copy_from_slice(b"ILEX-GEN2-DISK01");
        header[72..80].copy_from_slice(&table.to_le_bytes());
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        header[88..92].copy_from_slice(&crc.to_le_bytes());
        let checksum = crc32fast::hash(&header[..92]);
        header[16..20].copy_from_slice(&checksum.to_le_bytes());
    }
}

#[cfg(target_os = "linux")]
pub fn serial_payload() -> Vec<u8> {
    let mut code = vec![0x66, 0xba, 0xf8, 0x03]; // mov dx, 0x3f8
    for byte in b"ILEX-UNSIGNED-EXECUTED\r\n" {
        code.extend([0xb0, *byte, 0xee]);
    }
    code.extend([0xeb, 0xfe]);
    unsigned_efi(&code)
}
/// Minimal position-independent x64 EFI application. It emits a serial marker
/// and loops. Generated PE32+ has no Authenticode directory or external tools.
fn unsigned_efi(code: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 1024];
    fn u16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    b[..2].copy_from_slice(b"MZ");
    u32(&mut b, 0x3c, 0x80);
    b[0x80..0x84].copy_from_slice(b"PE\0\0");
    u16(&mut b, 0x84, 0x8664);
    u16(&mut b, 0x86, 1);
    u16(&mut b, 0x94, 240);
    u16(&mut b, 0x96, 0x22);
    let o = 0x98;
    u16(&mut b, o, 0x20b);
    u32(&mut b, o + 4, 512);
    u32(&mut b, o + 16, 0x1000);
    u32(&mut b, o + 20, 0x1000);
    b[o + 24..o + 32].copy_from_slice(&0x400000u64.to_le_bytes());
    u32(&mut b, o + 32, 4096);
    u32(&mut b, o + 36, 512);
    u16(&mut b, o + 40, 6);
    u16(&mut b, o + 48, 1);
    u32(&mut b, o + 56, 8192);
    u32(&mut b, o + 60, 512);
    u16(&mut b, o + 68, 10);
    u32(&mut b, o + 108, 16);
    let s = o + 240;
    b[s..s + 8].copy_from_slice(b".text\0\0\0");
    u32(&mut b, s + 8, 512);
    u32(&mut b, s + 12, 4096);
    u32(&mut b, s + 16, 512);
    u32(&mut b, s + 20, 512);
    u32(&mut b, s + 36, 0x60000020);
    b[512..512 + code.len()].copy_from_slice(code);
    b
}

/// An MBR ESP/FAT16 fixture, avoiding loop mounts or privileged host installs.
pub fn fat_disk(payload: &[u8]) -> Vec<u8> {
    let offset = 2048 * 512;
    let sectors = 32768usize;
    let mut disk = vec![0u8; offset + sectors * 512];
    disk[446 + 4] = 0xef;
    disk[454..458].copy_from_slice(&2048u32.to_le_bytes());
    disk[458..462].copy_from_slice(&(sectors as u32).to_le_bytes());
    disk[510..512].copy_from_slice(&[0x55, 0xaa]);
    let b = &mut disk[offset..];
    b[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    b[3..11].copy_from_slice(b"ILEXTEST");
    b[11..13].copy_from_slice(&512u16.to_le_bytes());
    b[13] = 4;
    b[14..16].copy_from_slice(&1u16.to_le_bytes());
    b[16] = 2;
    b[17..19].copy_from_slice(&512u16.to_le_bytes());
    b[19..21].copy_from_slice(&(sectors as u16).to_le_bytes());
    b[21] = 0xf8;
    b[22..24].copy_from_slice(&32u16.to_le_bytes());
    b[24..26].copy_from_slice(&63u16.to_le_bytes());
    b[26..28].copy_from_slice(&255u16.to_le_bytes());
    b[28..32].copy_from_slice(&2048u32.to_le_bytes());
    b[36] = 0x80;
    b[38] = 0x29;
    b[39..43].copy_from_slice(&1u32.to_le_bytes());
    b[43..54].copy_from_slice(b"ILEX EFI   ");
    b[54..62].copy_from_slice(b"FAT16   ");
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    for fat in [512, 33 * 512] {
        for (i, value) in [0xfff8u16, 0xffff, 0xffff, 0xffff, 0xffff]
            .iter()
            .enumerate()
        {
            b[fat + i * 2..fat + i * 2 + 2].copy_from_slice(&value.to_le_bytes());
        }
    }
    fn entry(b: &mut [u8], at: usize, name: &[u8; 11], dir: bool, cluster: u16, size: u32) {
        b[at..at + 11].copy_from_slice(name);
        b[at + 11] = if dir { 0x10 } else { 0x20 };
        b[at + 26..at + 28].copy_from_slice(&cluster.to_le_bytes());
        b[at + 28..at + 32].copy_from_slice(&size.to_le_bytes());
    }
    entry(b, 65 * 512, b"EFI        ", true, 2, 0);
    let data = 97 * 512;
    entry(b, data, b"BOOT       ", true, 3, 0);
    entry(
        b,
        data + 2048,
        b"BOOTX64 EFI",
        false,
        4,
        payload.len() as u32,
    );
    b[data + 4096..data + 4096 + payload.len()].copy_from_slice(payload);
    disk
}
