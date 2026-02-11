pub const BPB_SEC_SIZE: usize = 11;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct BiosParameterBlock {
    pub jmp_boot: [u8; 3],
    pub oem_name: [u8; 8],
    pub byts_per_sec: u16,
    pub sec_per_clus: u8,
    pub rsvd_sec_cnt: u16,
    pub num_fats: u8,
    pub root_ent_cnt: u16,
    pub tot_sec_16: u16,
    pub media: u8,
    pub fat_sz_16: u16,
    pub sec_per_trk: u16,
    pub num_heads: u16,
    pub hidd_sec: u32,
    pub tot_sec_32: u32,
    
    // FAT32 Structure
    pub fat_sz_32: u32,
    pub ext_flags: u16,
    pub fs_ver: u16,
    pub root_clus: u32,
    pub fs_info: u16,
    pub bk_boot_sec: u16,
    pub reserved: [u8; 12],
    pub drv_num: u8,
    pub reserved1: u8,
    pub boot_sig: u8,
    pub vol_id: u32,
    pub vol_lab: [u8; 11],
    pub fil_sys_type: [u8; 8],
}


pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DirEntry {
    pub name: [u8; 11],
    pub attr: u8,
    pub nt_res: u8,
    pub crt_time_tenth: u8,
    pub crt_time: u16,
    pub crt_date: u16,
    pub lst_acc_date: u16,
    pub fst_clus_hi: u16,
    pub wrt_time: u16,
    pub wrt_date: u16,
    pub fst_clus_lo: u16,
    pub file_size: u32,
}
