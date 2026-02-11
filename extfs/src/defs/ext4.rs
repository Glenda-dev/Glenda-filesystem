pub const SUPER_BLOCK_OFFSET: u64 = 1024;
pub const EXT4_SUPER_MAGIC: u16 = 0xEF53;

// Fixed inode numbers
pub const ROOT_INO: u32 = 2;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SuperBlock {
    pub s_inodes_count: u32,         // 0x0
    pub s_blocks_count_lo: u32,      // 0x4
    pub s_r_blocks_count_lo: u32,    // 0x8
    pub s_free_blocks_count_lo: u32, // 0xC
    pub s_free_inodes_count: u32,    // 0x10
    pub s_first_data_block: u32,     // 0x14
    pub s_log_block_size: u32,       // 0x18
    pub s_log_cluster_size: u32,     // 0x1C
    pub s_blocks_per_group: u32,     // 0x20
    pub s_clusters_per_group: u32,   // 0x24
    pub s_inodes_per_group: u32,     // 0x28
    pub s_mtime: u32,                // 0x2C
    pub s_wtime: u32,                // 0x30
    pub s_mnt_count: u16,            // 0x34
    pub s_max_mnt_count: u16,        // 0x36
    pub s_magic: u16,                // 0x38
    pub s_state: u16,                // 0x3A
    pub s_errors: u16,               // 0x3C
    pub s_minor_rev_level: u16,      // 0x3E
    pub s_lastcheck: u32,            // 0x40
    pub s_checkinterval: u32,        // 0x44
    pub s_creator_os: u32,           // 0x48
    pub s_rev_level: u32,            // 0x4C
    pub s_def_resuid: u16,           // 0x50
    pub s_def_resgid: u16,           // 0x52
    pub s_first_ino: u32,            // 0x54
    pub s_inode_size: u16,           // 0x58
    pub s_block_group_nr: u16,       // 0x5A
    pub s_feature_compat: u32,       // 0x5C
    pub s_feature_incompat: u32,     // 0x60
    pub s_feature_ro_compat: u32,    // 0x64
    pub s_uuid: [u8; 16],            // 0x68
    pub s_volume_name: [u8; 16],     // 0x78
    pub s_last_mounted: [u8; 64],    // 0x88
    pub s_algo_bitmap: u32,          // 0xC8
    // Performance hints
    pub s_prealloc_blocks: u8,      // 0xCC
    pub s_prealloc_dir_blocks: u8,  // 0xCD
    pub s_reserved_gdt_blocks: u16, // 0xCE
    // Journaling support
    pub s_journal_uuid: [u8; 16], // 0xD0
    pub s_journal_inum: u32,      // 0xE0
    pub s_journal_dev: u32,       // 0xE4
    pub s_last_orphan: u32,       // 0xE8
    pub s_hash_seed: [u32; 4],    // 0xEC
    pub s_def_hash_version: u8,   // 0xFC
    pub s_jnl_backup_type: u8,
    pub s_desc_size: u16,
    pub s_default_mount_opts: u32,
    pub s_first_meta_bg: u32,
    pub s_mkfs_time: u32,
    pub s_jnl_blocks: [u32; 17],
    // 64bit support
    pub s_blocks_count_hi: u32,
    pub s_r_blocks_count_hi: u32,
    pub s_free_blocks_count_hi: u32,
    pub s_min_extra_isize: u16,
    pub s_want_extra_isize: u16,
    pub s_flags: u32,
    pub s_raid_stride: u16,
    pub s_mmp_interval: u16,
    pub s_mmp_block: u64,
    pub s_raid_stripe_width: u32,
    pub s_log_groups_per_flex: u8,
    pub s_checksum_type: u8,
    pub s_reserved_pad: u16,
    pub s_kbytes_written: u64,
    pub s_snapshot_inum: u32,
    pub s_snapshot_id: u32,
    pub s_snapshot_r_blocks_count: u64,
    pub s_snapshot_list: u32,
    pub s_error_count: u32,
    pub s_first_error_time: u32,
    pub s_first_error_ino: u32,
    pub s_first_error_block: u64,
    pub s_first_error_func: [u8; 32],
    pub s_first_error_line: u32,
    pub s_last_error_time: u32,
    pub s_last_error_ino: u32,
    pub s_last_error_line: u32,
    pub s_last_error_block: u64,
    pub s_last_error_func: [u8; 32],
    pub s_mount_opts: [u8; 64],
    pub s_usr_quota_inum: u32,
    pub s_grp_quota_inum: u32,
    pub s_overhead_blocks: u32,
    pub s_backup_bgs: [u32; 2],
    pub s_encrypt_algos: [u8; 4],
    pub s_encrypt_pw_salt: [u8; 16],
    pub s_lpf_ino: u32,
    pub s_prj_quota_inum: u32,
    pub s_checksum_seed: u32,
    pub s_reserved: [u32; 98],
    pub s_checksum: u32,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct GroupDesc {
    pub bg_block_bitmap_lo: u32,
    pub bg_inode_bitmap_lo: u32,
    pub bg_inode_table_lo: u32,
    pub bg_free_blocks_count_lo: u16,
    pub bg_free_inodes_count_lo: u16,
    pub bg_used_dirs_count_lo: u16,
    pub bg_flags: u16,
    pub bg_exclude_bitmap_lo: u32,
    pub bg_block_bitmap_hi: u16,
    pub bg_inode_bitmap_hi: u16,
    pub bg_inode_table_hi: u16,
    pub bg_free_blocks_count_hi: u16,
    pub bg_free_inodes_count_hi: u16,
    pub bg_used_dirs_count_hi: u16,
    pub bg_pad: u16,
    pub bg_reserved: [u32; 3],
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Inode {
    pub i_mode: u16,
    pub i_uid: u16,
    pub i_size_lo: u32,
    pub i_atime: u32,
    pub i_ctime: u32,
    pub i_mtime: u32,
    pub i_dtime: u32,
    pub i_gid: u16,
    pub i_links_count: u16,
    pub i_blocks_lo: u32,
    pub i_flags: u32,
    pub i_osd1: u32,
    pub i_block: [u8; 60], // Extents
    pub i_generation: u32,
    pub i_file_acl_lo: u32,
    pub i_size_hi: u32,
    pub i_obso_faddr: u32,
    pub i_osd2: [u8; 12],
}
pub const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
pub const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
pub const EXT4_FEATURE_INCOMPAT_64BIT: u32 = 0x0080;
pub const EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const EXT4_EXTENTS_FL: u32 = 0x80000;
pub const EXT4_EXT_MAGIC: u16 = 0xF30A;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtentHeader {
    pub eh_magic: u16,
    pub eh_entries: u16,
    pub eh_max: u16,
    pub eh_depth: u16,
    pub eh_generation: u32,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Extent {
    pub ee_block: u32,
    pub ee_len: u16,
    pub ee_start_hi: u16,
    pub ee_start_lo: u32,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtentIndex {
    pub ei_block: u32,
    pub ei_leaf_lo: u32,
    pub ei_leaf_hi: u16,
    pub ei_unused: u16,
}

// Directory types
pub const EXT4_FT_UNKNOWN: u8 = 0;
pub const EXT4_FT_REG_FILE: u8 = 1;
pub const EXT4_FT_DIR: u8 = 2;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DirEntry2 {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    // Name follows
}
