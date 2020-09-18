use winapi::um::unknwnbase::IUnknown;

use winapi::ctypes::c_void;
pub use winapi::shared::d3d9::*;
pub use winapi::shared::d3d9types::*;
pub use winapi::shared::minwindef::*;
pub use winapi::shared::windef::{HWND, RECT};
pub use winapi::shared::winerror::{E_FAIL, S_OK};
use winapi::um::wingdi::RGNDATA;
pub use winapi::um::winnt::{HRESULT, LPCWSTR};

use fnv::FnvHashMap;
use fnv::FnvHashSet;

use dnclr::{init_clr, reload_managed_dll};
use input;
use interop;
use interop::InteropState;
use interop::NativeModData;
use util;
use util::*;
use constant_tracking;
use shader_capture;
use d3dx;

use std;
use std::fmt;
use std::ptr::null_mut;
use std::time::SystemTime;

use shared_dx9::defs::*;
use shared_dx9::types::*;
use shared_dx9::util::*;
use shared_dx9::error::*;

const CLR_OK:u64 = 1;
const CLR_FAIL:u64 = 666;

const MAX_STAGE: usize = 16;

// Snapshotting currently stops after a certain amount of real time has passed from the start of
// the snap, specified by this constant.
// One might expect that just snapping everything drawn within a single begin/end scene combo is
// sufficient, but this often misses data,
// and sometimes fails to snapshot anything at all.  This may be because the game is using multiple
// begin/end combos, so maybe
// present->present would be more reliable (TODO: check this)
// Using a window makes it much more likely that something useful is captured, at the expense of
// some duplicates; even though
// some objects may still be missed.  Some investigation to make this more reliable would be useful.
static SNAP_MS: u32 = 250;

pub struct FrameMetrics {
    pub dip_calls: u32,
    pub frames: u32,
    pub last_call_log: SystemTime,
    pub last_frame_log: SystemTime,
    pub last_fps: f64,
    pub last_fps_update: SystemTime,
    pub low_framerate: bool,
}

pub struct HookState {
    pub clr_pointer: Option<u64>,
    pub interop_state: Option<InteropState>,
    //pub is_global: bool,
    pub loaded_mods: Option<FnvHashMap<u32, Vec<interop::NativeModData>>>,
    pub mods_by_name: Option<FnvHashMap<String,u32>>,
    // lists of pointers containing the set of textures in use during snapshotting.
    // these are simply compared against the selection texture, never dereferenced.
    pub active_texture_set: Option<FnvHashSet<*mut IDirect3DBaseTexture9>>,
    pub active_texture_list: Option<Vec<*mut IDirect3DBaseTexture9>>,
    pub making_selection: bool,
    pub in_dip: bool,
    pub in_hook_release: bool,
    pub in_beginend_scene: bool,
    pub show_mods: bool,
    pub mm_root: Option<String>,
    pub input: Option<input::Input>,
    pub selection_texture: *mut IDirect3DTexture9,
    pub selected_on_stage: [bool; MAX_STAGE],
    pub curr_texture_index: usize,
    pub is_snapping: bool,
    pub snap_start: SystemTime,
    pub d3dx_fn: Option<d3dx::D3DXFn>,
    pub device: Option<*mut IDirect3DDevice9>, // only valid during snapshots
    pub metrics: FrameMetrics,
    pub vertex_constants: Option<constant_tracking::ConstantGroup>,
    pub pixel_constants: Option<constant_tracking::ConstantGroup>,
}

impl HookState {
    pub fn in_any_hook_fn(&self) -> bool {
        self.in_dip || self.in_hook_release || self.in_beginend_scene
    }
}
impl fmt::Display for HookState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "HookState (thread: {:?})", // : d3d9: {:?}, device: {:?}",
            std::thread::current().id(),
            //self.hook_direct3d9.is_some(),
            //self.hook_direct3d9device.is_some()
        )
    }
}

lazy_static! {
    pub static ref GLOBAL_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
pub static mut DEVICE_STATE: *mut DeviceState = null_mut();

pub fn dev_state() -> &'static mut DeviceState {
    unsafe {
        if DEVICE_STATE == null_mut() {
            write_log_file("accessing null device state pointer, this 'should never happen'.  we gonna crash boys");
            panic!("Aborting because I'm about to dereference a null device state pointer.");
        }
        &mut (*DEVICE_STATE)
    }
}

// TODO: maybe create read/write accessors for this
// TODO: actually the way global state is handled is super gross.  at a minimum it seems 
// like it should be a behind a RW lock, and if I made it a pointer/box I could get rid of some 
// of the option types that are only there due to Rust limitations on what can be used to 
// init constants.
pub static mut GLOBAL_STATE: HookState = HookState {
    clr_pointer: None,
    interop_state: None,
    //is_global: true,
    loaded_mods: None,
    mods_by_name: None,
    active_texture_set: None,
    active_texture_list: None,
    making_selection: false,
    in_dip: false,
    in_hook_release: false,
    in_beginend_scene: false,
    show_mods: true,
    mm_root: None,
    input: None,
    selection_texture: null_mut(),
    selected_on_stage: [false; MAX_STAGE],
    curr_texture_index: 0,
    is_snapping: false,
    snap_start: std::time::UNIX_EPOCH,
    vertex_constants: None,
    pixel_constants: None,

    d3dx_fn: None,
    device: None,
    metrics: FrameMetrics {
        dip_calls: 0,
        frames: 0,
        last_call_log: std::time::UNIX_EPOCH,
        last_frame_log: std::time::UNIX_EPOCH,
        last_fps_update: std::time::UNIX_EPOCH,
        last_fps: 120.0,
        low_framerate: false,
    },
};

macro_rules! impl_release_drop {
    ($ptrtype:ident) => {
        impl ReleaseDrop for *mut $ptrtype {
            fn OnDrop(&mut self) -> () {
                unsafe {
                    let ptr = *self;
                    if ptr != null_mut() {
                        (*ptr).Release();
                    }
                };
            }
        }
    };
}

impl_release_drop!(IDirect3DBaseTexture9);
impl_release_drop!(IDirect3DVertexDeclaration9);
impl_release_drop!(IDirect3DIndexBuffer9);
impl_release_drop!(IDirect3DPixelShader9);
impl_release_drop!(IDirect3DVertexShader9);
impl_release_drop!(ID3DXBuffer);

enum AsyncLoadState {
    NotStarted = 51,
    Pending,
    InProgress,
    Complete,
}

fn snapshot_extra() -> bool {
    return constant_tracking::is_enabled() || shader_capture::is_enabled()
}

fn get_current_texture() -> *mut IDirect3DBaseTexture9 {
    unsafe {
        let idx = GLOBAL_STATE.curr_texture_index;
        GLOBAL_STATE
            .active_texture_list
            .as_ref()
            .map(|list| {
                if idx >= list.len() {
                    null_mut()
                } else {
                    list[idx]
                }
            })
            .unwrap_or(null_mut())
    }
}

fn get_selected_texture_stage() -> Option<DWORD> {
    unsafe {
        for i in 0..MAX_STAGE {
            if GLOBAL_STATE.selected_on_stage[i] {
                return Some(i as DWORD);
            }
        }
        None
    }
}

pub fn get_global_state_ptr() -> *mut HookState {
    let pstate: *mut HookState = unsafe { &mut GLOBAL_STATE };
    pstate
}

unsafe fn clear_loaded_mods(device: *mut IDirect3DDevice9) {
    let lock = GLOBAL_STATE_LOCK.lock();
    if let Err(_e) = lock {
        write_log_file("failed to lock global state to clear mod data");
        return;
    }

    // get device ref count prior to adding everything
    (*device).AddRef();
    let pre_rc = (*device).Release();

    let mods = GLOBAL_STATE.loaded_mods.take();
    let mut cnt = 0;
    mods.map(|mods| {
        for (_key, modvec) in mods.into_iter() {
            for nmd in modvec {
                cnt += 1;
                if nmd.vb != null_mut() {
                    (*nmd.vb).Release();
                }
                if nmd.ib != null_mut() {
                    (*nmd.ib).Release();
                }
                if nmd.decl != null_mut() {
                    (*nmd.decl).Release();
                }
                
                for tex in nmd.textures.iter() {
                    if *tex != null_mut() {
                        let tex = *tex as *mut IDirect3DBaseTexture9;
                        (*tex).Release();
                    }
                }
            }
        }
    });
    GLOBAL_STATE.mods_by_name = None;

    (*device).AddRef();
    let post_rc = (*device).Release();
    let diff = pre_rc - post_rc;
    if (dev_state().d3d_resource_count as i64 - diff as i64) < 0 {
        write_log_file(&format!(
            "DOH resource count would go below zero (curr: {}, removed {}),",
            dev_state().d3d_resource_count, diff
        ));
    } else {
        dev_state().d3d_resource_count -= diff;
    }

    write_log_file(&format!("unloaded {} mods", cnt));
}

unsafe fn setup_mod_data(device: *mut IDirect3DDevice9, callbacks: interop::ManagedCallbacks) {
    clear_loaded_mods(device);

    let mod_count = (callbacks.GetModCount)();
    if mod_count <= 0 {
        return;
    }

    if device == null_mut() {
        return;
    }

    let lock = GLOBAL_STATE_LOCK.lock();
    if let Err(_e) = lock {
        write_log_file("failed to lock global state to setup mod data");
        return;
    }
    
    // need d3dx for textures
    GLOBAL_STATE.device = Some(device);
    if GLOBAL_STATE.d3dx_fn.is_none() {
        GLOBAL_STATE.d3dx_fn = d3dx::load_lib(&GLOBAL_STATE.mm_root)
            .map_err(|e| {
                write_log_file(&format!(
                    "failed to load d3dx: texture snapping not available: {:?}",
                    e
                ));
                e
            })
            .ok();
    }

    // get device ref count prior to adding everything
    (*device).AddRef();
    let pre_rc = (*device).Release();

    let mut loaded_mods: FnvHashMap<u32, Vec<interop::NativeModData>> =
        FnvHashMap::with_capacity_and_hasher((mod_count * 10) as usize, Default::default());
    // map of modname -> mod key, which can then be indexed into loaded mods.  used by 
    // child mods to find the parent.
    let mut mods_by_name: FnvHashMap<String,u32> = 
        FnvHashMap::with_capacity_and_hasher((mod_count * 10) as usize, Default::default());

    // temporary list of all mods that have been referenced as a parent by something
    let mut parent_mods:HashSet<String> = HashSet::new();
    for midx in 0..mod_count {
        let mdat: *mut interop::ModData = (callbacks.GetModData)(midx);

        if mdat == null_mut() {
            write_log_file(&format!("null mod at index {}", midx));
            continue;
        }
        let mod_name = util::from_wide_str(&(*mdat).modName).unwrap_or_else(|_e| "".to_owned());
        let mod_name = mod_name.trim().to_owned();
        let parent_mod = util::from_wide_str(&(*mdat).parentModName).unwrap_or_else(|_e| "".to_owned());
        let parent_mod = parent_mod.trim().to_owned();
        let (prims,verts) = if (*mdat).numbers.mod_type == (interop::ModType::Deletion as i32) {
            ((*mdat).numbers.ref_prim_count as u32,(*mdat).numbers.ref_vert_count as u32)
        } else {
            ((*mdat).numbers.prim_count as u32, (*mdat).numbers.vert_count as u32)
        };
        write_log_file(&format!("==> Initializing mod: name '{}', parent '{}', type {}, prims {}, verts {}", 
            mod_name, parent_mod, (*mdat).numbers.mod_type, prims, verts));
        let mod_type = (*mdat).numbers.mod_type;
        if mod_type != interop::ModType::GPUReplacement as i32
            && mod_type != interop::ModType::Deletion as i32
        {
            write_log_file(&format!(
                "Unsupported mod type: {}",
                (*mdat).numbers.mod_type
            ));
            continue;
        }

        // names are case insensitive
        let mod_name = mod_name.to_lowercase();
        let mut native_mod_data = interop::NativeModData {
            mod_data: (*mdat),
            vb_data: null_mut(),
            ib_data: null_mut(),
            decl_data: null_mut(),
            vb: null_mut(),
            ib: null_mut(),
            decl: null_mut(),
            textures: [null_mut(); 4],
            is_parent: false,
            parent_mod_name: "".to_owned(),
            last_frame_render: 0,
            name: mod_name.to_owned(),
        };

        if (*mdat).numbers.mod_type == (interop::ModType::Deletion as i32) {
            let hash_code = NativeModData::mod_key(
                native_mod_data.mod_data.numbers.ref_vert_count as u32,
                native_mod_data.mod_data.numbers.ref_prim_count as u32,
            );

            loaded_mods.entry(hash_code).or_insert(vec![]).push(native_mod_data);
            // thats all we need to do for these.
            continue;
        }

        let decl_size = (*mdat).numbers.decl_size_bytes;
        // vertex declaration construct copies the vec bytes, so just keep a temp vector reference for the data
        let (decl_data, _decl_vec) = if decl_size > 0 {
            let mut decl_vec: Vec<u8> = Vec::with_capacity(decl_size as usize);
            let decl_data: *mut u8 = decl_vec.as_mut_ptr();
            (decl_data, Some(decl_vec))
        } else {
            (null_mut(), None)
        };

        let vb_size = (*mdat).numbers.prim_count * 3 * (*mdat).numbers.vert_size_bytes;
        let mut vb_data: *mut u8 = null_mut();

        // index buffers not currently supported
        let ib_size = 0; //mdat->indexCount * mdat->indexElemSizeBytes;
        let ib_data: *mut u8 = null_mut();

        // create vb
        let mut out_vb: *mut IDirect3DVertexBuffer9 = null_mut();
        let out_vb: *mut *mut IDirect3DVertexBuffer9 = &mut out_vb;
        let hr = (*device).CreateVertexBuffer(
            vb_size as UINT,
            D3DUSAGE_WRITEONLY,
            0,
            D3DPOOL_MANAGED,
            out_vb,
            null_mut(),
        );
        if hr != 0 {
            write_log_file(&format!(
                "failed to create vertex buffer for mod {}: HR {:x}",
                midx, hr
            ));
            return;
        }

        let vb = *out_vb;

        // lock vb to obtain write buffer
        let hr = (*vb).Lock(0, 0, std::mem::transmute(&mut vb_data), 0);
        if hr != 0 {
            write_log_file(&format!("failed to lock vertex buffer: {:x}", hr));
            return;
        }

        // fill all data buckets with managed code
        let ret = (callbacks.FillModData)(
            midx, decl_data, decl_size, vb_data, vb_size, ib_data, ib_size,
        );

        let hr = (*vb).Unlock();
        if hr != 0 {
            write_log_file(&format!("failed to unlock vertex buffer: {:x}", hr));
            (*vb).Release();
            return;
        }

        if ret != 0 {
            write_log_file(&format!("failed to fill mod data: {}", ret));
            (*vb).Release();
            return;
        }

        native_mod_data.vb = vb;

        // create vertex declaration
        let mut out_decl: *mut IDirect3DVertexDeclaration9 = null_mut();
        let pp_out_decl: *mut *mut IDirect3DVertexDeclaration9 = &mut out_decl;
        let hr =
            (*device).CreateVertexDeclaration(decl_data as *const D3DVERTEXELEMENT9, pp_out_decl);
        if hr != 0 {
            write_log_file(&format!("failed to create vertex declaration: {}", hr));
            (*vb).Release();
            return;
        }
        if out_decl == null_mut() {
            write_log_file("vertex declaration is null");
            (*vb).Release();
            return;
        }
        native_mod_data.decl = out_decl;

        let load_tex = |texpath:&[u16]| {
            let tex = util::from_wide_str(texpath).unwrap_or_else(|e| {
                write_log_file(&format!("failed to load texture: {:?}", e));
                "".to_owned()
            });
            let tex = tex.trim();
            
            let mut outtex = null_mut();
            if !tex.is_empty() {
                outtex = d3dx::load_texture(texpath.as_ptr()).map_err(|e| {
                    write_log_file(&format!("failed to load texture: {:?}", e));
                }).unwrap_or(null_mut());
                write_log_file(&format!("Loaded tex: {:?} {:x}", tex, outtex as u64));
            }
            outtex
        };
        native_mod_data.textures[0] = load_tex(&(*mdat).texPath0);
        native_mod_data.textures[1] = load_tex(&(*mdat).texPath1);
        native_mod_data.textures[2] = load_tex(&(*mdat).texPath2);
        native_mod_data.textures[3] = load_tex(&(*mdat).texPath3);

        let mod_key = NativeModData::mod_key(
            native_mod_data.mod_data.numbers.ref_vert_count as u32,
            native_mod_data.mod_data.numbers.ref_prim_count as u32,
        );
        if !parent_mod.is_empty() {
            native_mod_data.parent_mod_name = parent_mod.to_lowercase();
            parent_mods.insert(native_mod_data.parent_mod_name.clone());
        }
        // TODO: is hashing the generated mod key better than just hashing a tuple of prims,verts?
        loaded_mods.entry(mod_key).or_insert(vec![]).push(native_mod_data);
        if !mod_name.is_empty() {
            if mods_by_name.contains_key(&mod_name) {
                write_log_file(&format!("error, duplicate mod name: ignoring dup: {}", mod_name));
            } else {
                mods_by_name.insert(mod_name, mod_key);
            }
        }
        write_log_file(&format!(
            "allocated vb/decl for mod data {}: {:?}",
            midx,
            (*mdat).numbers
        ));
    }
    
    // mark all parent mods as such, and also warn about any parents that didn't load
    let mut resolved_parents = 0;
    let num_parents = parent_mods.len();
    for parent in parent_mods {
        match mods_by_name.get(&parent) {
            None => write_log_file(&format!("error, mod referenced as parent failed to load: {}", parent)),
            Some(modkey) => {
                match loaded_mods.get_mut(modkey) {
                    None => write_log_file(&format!("error, mod referenced as parent was found, but no loaded: {}", parent)),
                    Some(nmdatavec) => {
                        for nmdata in nmdatavec.iter_mut() {
                            if nmdata.name == parent {
                                resolved_parents += 1;
                                nmdata.is_parent = true
                            }
                        }
                    }
                }
            }
        }
    }
    write_log_file(&format!("resolved {} of {} parent mods", resolved_parents, num_parents));
    
    // verify that all multi-mod cases have parent mod names set.
    for nmodv in loaded_mods.values() {
        if nmodv.len() <= 1 {
            continue
        }
        // in a multimod case, all mods must have parents, and the parents have to be different
        // names.
        let mut parent_names: HashSet<String> = HashSet::new();
        for nmod in nmodv.iter() {
            if nmod.parent_mod_name.is_empty() {
                write_log_file(&format!("Error: mod '{}' ({} prims,{} verts) has no parent \
mod but it overlaps with another mod.  This won't render correctly.",
                nmod.name, nmod.mod_data.numbers.prim_count, nmod.mod_data.numbers.vert_count));
            } else {
                parent_names.insert(nmod.parent_mod_name.to_string());
            }
            
        }
        if nmodv.len() != parent_names.len() {
            write_log_file("Error: mod overlap found, check that all of these mods have proper/unique parents:");
            for nmod in nmodv.iter() {
                write_log_file(&format!("  mod: {}, prims: {}, verts: {}, parent: {}",
                nmod.name, nmod.mod_data.numbers.prim_count, nmod.mod_data.numbers.vert_count, nmod.parent_mod_name));
            }
        }
    }

    // get new ref count
    (*device).AddRef();
    let post_rc = (*device).Release();
    let diff = post_rc - pre_rc;
    (*DEVICE_STATE).d3d_resource_count += diff;
    write_log_file(&format!(
        "mod loading added {} to device {:x} ref count, new count: {}",
        diff, device as u64, (*DEVICE_STATE).d3d_resource_count
    ));

    GLOBAL_STATE.loaded_mods = Some(loaded_mods);
    GLOBAL_STATE.mods_by_name = Some(mods_by_name);
}

pub fn do_per_frame_operations(device: *mut IDirect3DDevice9) -> Result<()> {
    // init the clr if needed
    {
        let hookstate = unsafe { &mut GLOBAL_STATE };
        if hookstate.clr_pointer.is_none() {
            let lock = GLOBAL_STATE_LOCK.lock();
            match lock {
                Ok(_ignored) => {
                    if hookstate.clr_pointer.is_none() {
                        // store something in clr_pointer even if it create fails,
                        // so that we don't keep trying to create it.  clr_pointer is
                        // really just a bool right now, it remains to be
                        // seen whether storing anything related to clr in
                        // global state is actually useful.
                        write_log_file("creating CLR");
                        init_clr(&hookstate.mm_root)
                            .and_then(|_x| {
                                reload_managed_dll(&hookstate.mm_root)
                            })
                            .and_then(|_x| {
                                hookstate.clr_pointer = Some(CLR_OK);
                                Ok(_x)
                            })
                            .map_err(|e| {
                                write_log_file(&format!("Error creating CLR: {:?}", e));
                                hookstate.clr_pointer = Some(CLR_FAIL);
                                e
                            })?;
                    }
                }
                Err(e) => write_log_file(&format!("{:?} should never happen", e)),
            };
        }
    }
    // write_log_file(&format!("performing per-scene ops on thread {:?}",
    //         std::thread::current().id()));

    let interop_state = unsafe { &mut GLOBAL_STATE.interop_state };

    interop_state.as_mut().map(|is| {
        if !is.loading_mods && !is.done_loading_mods && is.conf_data.LoadModsOnStart {
            let loadstate = unsafe { (is.callbacks.GetLoadingState)() };
            if loadstate == AsyncLoadState::InProgress as i32 {
                is.loading_mods = true;
                is.done_loading_mods = false;
            } else if loadstate != AsyncLoadState::Pending as i32 {
                let r = unsafe { (is.callbacks.LoadModDB)() };
                if r == AsyncLoadState::Pending as i32 {
                    is.loading_mods = true;
                    is.done_loading_mods = false;
                }
                if r == AsyncLoadState::Complete as i32 {
                    is.loading_mods = false;
                    is.done_loading_mods = true;
                }
                write_log_file(&format!("mod db load returned: {}", r));
            }
        }

        if is.loading_mods
            && unsafe { (is.callbacks.GetLoadingState)() } == AsyncLoadState::Complete as i32
        {
            write_log_file("mod loading complete");
            is.loading_mods = false;
            is.done_loading_mods = true;

            unsafe { setup_mod_data(device, is.callbacks) };
        }
    });
    Ok(())
}

unsafe extern "system" fn hook_set_texture(
    THIS: *mut IDirect3DDevice9,
    Stage: DWORD,
    pTexture: *mut IDirect3DBaseTexture9,
) -> HRESULT {
    let has_it = GLOBAL_STATE
        .active_texture_set
        .as_ref()
        .map(|set| set.contains(&pTexture))
        .unwrap_or(true);
    if !has_it {
        GLOBAL_STATE.active_texture_set.as_mut().map(|set| {
            set.insert(pTexture);
        });
        GLOBAL_STATE.active_texture_list.as_mut().map(|list| {
            list.push(pTexture);
        });
    }

    if Stage < MAX_STAGE as u32 {
        let curr = get_current_texture();
        if curr != null_mut() && pTexture == curr {
            GLOBAL_STATE.selected_on_stage[Stage as usize] = true;
        } else if GLOBAL_STATE.selected_on_stage[Stage as usize] {
            GLOBAL_STATE.selected_on_stage[Stage as usize] = false;
        }
    }

    (dev_state().hook_direct3d9device.as_ref().unwrap().real_set_texture)(THIS, Stage, pTexture)
}

fn init_selection_mode(device: *mut IDirect3DDevice9) -> Result<()> {
    let hookstate = unsafe { &mut GLOBAL_STATE };
    hookstate.making_selection = true;
    hookstate.active_texture_list = Some(Vec::with_capacity(5000));
    hookstate.active_texture_set = Some(FnvHashSet::with_capacity_and_hasher(
        5000,
        Default::default(),
    ));

    unsafe {
        // hot-patch the snapshot hook functions
        let vtbl: *mut IDirect3DDevice9Vtbl = std::mem::transmute((*device).lpVtbl);
        let vsize = std::mem::size_of::<IDirect3DDevice9Vtbl>();

        let old_prot = unprotect_memory(vtbl as *mut c_void, vsize)?;

        // TODO: should hook SetStreamSource so that we can tell what streams are in use
        (*vtbl).SetTexture = hook_set_texture;

        protect_memory(vtbl as *mut c_void, vsize, old_prot)?;
    }
    Ok(())
}

fn init_snapshot_mode() {
    unsafe {
        if GLOBAL_STATE.is_snapping {
            return;
        }
        GLOBAL_STATE.is_snapping = true;
        GLOBAL_STATE.snap_start = SystemTime::now();
    }
}

fn cmd_select_next_texture(device: *mut IDirect3DDevice9) {
    let hookstate = unsafe { &mut GLOBAL_STATE };
    if !hookstate.making_selection {
        init_selection_mode(device)
            .unwrap_or_else(|_e| write_log_file("woops couldn't init selection mode"));
    }

    let len = hookstate
        .active_texture_list
        .as_mut()
        .map(|list| list.len())
        .unwrap_or(0);

    if len == 0 {
        return;
    }

    hookstate.curr_texture_index += 1;
    if hookstate.curr_texture_index >= len {
        hookstate.curr_texture_index = 0;
    }
}
fn cmd_select_prev_texture(device: *mut IDirect3DDevice9) {
    let hookstate = unsafe { &mut GLOBAL_STATE };
    if !hookstate.making_selection {
        init_selection_mode(device)
            .unwrap_or_else(|_e| write_log_file("woops couldn't init selection mode"));
    }

    let len = hookstate
        .active_texture_list
        .as_mut()
        .map(|list| list.len())
        .unwrap_or(0);

    if len == 0 {
        return;
    }

    hookstate.curr_texture_index = hookstate.curr_texture_index.wrapping_sub(1);
    if hookstate.curr_texture_index >= len {
        hookstate.curr_texture_index = len - 1;
    }
}
fn cmd_clear_texture_lists(_device: *mut IDirect3DDevice9) {
    unsafe {
        GLOBAL_STATE
            .active_texture_list
            .as_mut()
            .map(|list| list.clear());
        GLOBAL_STATE
            .active_texture_set
            .as_mut()
            .map(|list| list.clear());
        GLOBAL_STATE.curr_texture_index = 0;
        for i in 0..MAX_STAGE {
            GLOBAL_STATE.selected_on_stage[i] = false;
        }
        GLOBAL_STATE.making_selection = false;

        // TODO: this was an attempt to fix the issue with the selection
        // texture getting clobbered after alt-tab, but it didn't work.
        // if GLOBAL_STATE.selection_texture != null_mut() {
        //     let mut tex: *mut IDirect3DTexture9 = GLOBAL_STATE.selection_texture;
        //     if tex != null_mut() {
        //         (*tex).Release();
        //     }
        //     GLOBAL_STATE.selection_texture = null_mut();
        //     create_selection_texture(device);
        // }
    }
}
fn cmd_toggle_show_mods() {
    let hookstate = unsafe { &mut GLOBAL_STATE };
    hookstate.show_mods = !hookstate.show_mods;
}
fn cmd_take_snapshot() {
    init_snapshot_mode();
}

fn is_loading_mods() -> bool {
    let interop_state = unsafe { &mut GLOBAL_STATE.interop_state };
    let loading = interop_state.as_mut().map(|is| {
        if is.loading_mods {
            return true;
        }
        let loadstate = unsafe { (is.callbacks.GetLoadingState)() };
        if loadstate == AsyncLoadState::InProgress as i32 {
            return true;
        }
        false
    }).unwrap_or(false);
    loading
}

fn cmd_clear_mods(device: *mut IDirect3DDevice9) {
    if is_loading_mods() {
        write_log_file("cannot reload now; mods are loading");
        return;
    }
    let interop_state = unsafe { &mut GLOBAL_STATE.interop_state };
    interop_state.as_mut().map(|is| {
        write_log_file("clearing mods");
        is.loading_mods = false;
        is.done_loading_mods = true;

        unsafe {
            clear_loaded_mods(device);
        }
    });
}

fn cmd_reload_mods(device: *mut IDirect3DDevice9) {
    if is_loading_mods() {
        write_log_file("cannot reload now; mods are loading");
        return;
    }
    cmd_clear_mods(device);
    let interop_state = unsafe { &mut GLOBAL_STATE.interop_state };
    interop_state.as_mut().map(|is| {
        write_log_file("reloading mods");
        is.loading_mods = false;
        is.done_loading_mods = false;

        // the actual reload will be handled in per-frame operations
    });
}

fn cmd_reload_managed_dll(device: *mut IDirect3DDevice9) {
    if is_loading_mods() {
        write_log_file("cannot reload now; mods are loading");
        return;
    }
    unsafe {clear_loaded_mods(device)};
    // TODO: should check for active snapshotting and anything else that might be using the managed
    // code
    let hookstate = unsafe { &mut GLOBAL_STATE };
    match hookstate.clr_pointer {
        Some(CLR_OK) => {
            let res = reload_managed_dll(&hookstate.mm_root);
            match res {
                Ok(_) => write_log_file("managed dll reloaded"),
                Err(e) => write_log_file(&format!("ERROR: reloading managed dll failed: {:?}", e))
            }
        },
        _ => ()
    };
}

fn setup_fkey_input(device: *mut IDirect3DDevice9, inp: &mut input::Input) {
    write_log_file("using fkey input layout");
    // If you change these, be sure to change LocStrings/ProfileText in MMLaunch!
    // _fKeyMap[DIK_F1] = [&]() { this->loadMods(); };
    // _fKeyMap[DIK_F7] = [&]() { this->requestSnap(); };

    // Allow the handlers to take a copy of the device pointer in the closure.
    // This means that these handlers must be cleared when the device is destroyed,
    // (see purge_device_resources)
    // but lets us avoid passing a context argument through the input layer.
    inp.add_press_fn(input::DIK_F1, Box::new(move || cmd_reload_mods(device)));
    inp.add_press_fn(input::DIK_F2, Box::new(|| cmd_toggle_show_mods()));
    inp.add_press_fn(
        input::DIK_F3,
        Box::new(move || cmd_select_next_texture(device)),
    );
    inp.add_press_fn(
        input::DIK_F4,
        Box::new(move || cmd_select_prev_texture(device)),
    );
    inp.add_press_fn(input::DIK_F6, Box::new(move || cmd_clear_texture_lists(device)));
    inp.add_press_fn(input::DIK_F7, Box::new(move || cmd_take_snapshot()));
    // Disabling this because its ineffective: the reload will complete without error, but
    // The old managed code will still be used.  The old C++ code
    // used a custom domain manager to support reloading, but I'd rather just move to the
    // CoreCLR rather than reimplement that.
    //inp.add_press_fn(input::DIK_F10, Box::new(move || cmd_reload_managed_dll(device)));
}

fn setup_punct_input(_device: *mut IDirect3DDevice9, _inp: &mut input::Input) {
    write_log_file("using punct key input layout");
    // If you change these, be sure to change LocStrings/ProfileText in MMLaunch!
    // TODO: hook these up
    // _punctKeyMap[DIK_BACKSLASH] = [&]() { this->loadMods(); };
    // _punctKeyMap[DIK_RBRACKET] = [&]() { this->toggleShowModMesh(); };
    // _punctKeyMap[DIK_SEMICOLON] = [&]() { this->clearTextureLists(); };
    // _punctKeyMap[DIK_COMMA] = [&]() { this->selectNextTexture(); };
    // _punctKeyMap[DIK_PERIOD] = [&]() { this->selectPrevTexture(); };
    // _punctKeyMap[DIK_SLASH] = [&]() { this->requestSnap(); };
    // _punctKeyMap[DIK_MINUS] = [&]() { this->loadEverything(); };
}

fn setup_input(device: *mut IDirect3DDevice9, inp: &mut input::Input) -> Result<()> {
    use std::ffi::CStr;

    // Set key bindings.  Input also assumes that CONTROL modifier is required for these as well.
    // TODO: should push this out to conf file eventually so that they can be customized without rebuild
    let interop_state = unsafe { &GLOBAL_STATE.interop_state };
    interop_state
        .as_ref()
        .ok_or(HookError::DInputCreateFailed(String::from(
            "no interop state",
        )))
        .and_then(|is| {
            let carr_ptr = &is.conf_data.InputProfile[0] as *const i8;
            unsafe { CStr::from_ptr(carr_ptr) }
                .to_str()
                .map_err(|e| HookError::CStrConvertFailed(e))
        })
        .and_then(|inp_profile| {
            let lwr = inp_profile.to_owned().to_lowercase();
            if lwr.starts_with("fk") {
                setup_fkey_input(device, inp);
            } else if lwr.starts_with("punct") {
                setup_punct_input(device, inp);
            } else {
                write_log_file(&format!(
                    "input scheme unrecognized: {}, using FKeys",
                    inp_profile
                ));
                setup_fkey_input(device, inp);
            }
            Ok(())
        })
}

fn create_selection_texture(device: *mut IDirect3DDevice9) {
    unsafe {
        let width = 256;
        let height = 256;

        (*device).AddRef();
        let pre_rc = (*device).Release();

        let mut tex: *mut IDirect3DTexture9 = null_mut();
        let hr = (*device).CreateTexture(
            width,
            height,
            1,
            0,
            D3DFMT_A8R8G8B8,
            D3DPOOL_MANAGED,
            &mut tex,
            null_mut(),
        );
        if hr != 0 {
            write_log_file(&format!("failed to create selection texture: {:x}", hr));
            return;
        }

        // fill it with a lovely shade of green
        let mut rect: D3DLOCKED_RECT = std::mem::zeroed();
        let hr = (*tex).LockRect(0, &mut rect, null_mut(), D3DLOCK_DISCARD);
        if hr != 0 {
            write_log_file(&format!("failed to lock selection texture: {:x}", hr));
            (*tex).Release();
            return;
        }

        let dest: *mut u32 = std::mem::transmute(rect.pBits);
        for i in 0..width * height {
            let d: *mut u32 = dest.offset(i as isize);
            *d = 0xFF00FF00;
        }
        let hr = (*tex).UnlockRect(0);
        if hr != 0 {
            write_log_file("failed to unlock selection texture");
            (*tex).Release();
            return;
        }
        write_log_file("created selection texture");

        (*device).AddRef();
        let post_rc = (*device).Release();
        let diff = post_rc - pre_rc;

        dev_state().d3d_resource_count += diff;

        GLOBAL_STATE.selection_texture = tex;
    }
}

// TODO: hook this up to device release at the proper time
unsafe fn purge_device_resources(device: *mut IDirect3DDevice9) {
    if device == null_mut() {
        write_log_file("WARNING: ignoring insane attempt to purge devices on a null device");
        return;
    }
    clear_loaded_mods(device);
    if GLOBAL_STATE.selection_texture != null_mut() {
        (*GLOBAL_STATE.selection_texture).Release();
        GLOBAL_STATE.selection_texture = null_mut();
    }
    GLOBAL_STATE
        .input
        .as_mut()
        .map(|input| input.clear_handlers());
    dev_state().d3d_resource_count = 0;
}

pub unsafe extern "system" fn hook_present(
    THIS: *mut IDirect3DDevice9,
    pSourceRect: *const RECT,
    pDestRect: *const RECT,
    hDestWindowOverride: HWND,
    pDirtyRegion: *const RGNDATA,
) -> HRESULT {
    //write_log_file("present");
    if GLOBAL_STATE.in_any_hook_fn() {
        return (dev_state().hook_direct3d9device.as_ref().unwrap().real_present)(
            THIS,
            pSourceRect,
            pDestRect,
            hDestWindowOverride,
            pDirtyRegion,
        );
    }

    if let Err(e) = do_per_frame_operations(THIS) {
        write_log_file(&format!(
            "unexpected error from do_per_scene_operations: {:?}",
            e
        ));
        return (dev_state().hook_direct3d9device.as_ref().unwrap().real_present)(
            THIS,
            pSourceRect,
            pDestRect,
            hDestWindowOverride,
            pDirtyRegion,
        );
    }

    let min_fps = GLOBAL_STATE
        .interop_state
        .map(|is| is.conf_data.MinimumFPS)
        .unwrap_or(0) as f64;

    let metrics = &mut GLOBAL_STATE.metrics;
    let present_ret = dev_state()
        .hook_direct3d9device
        .as_mut()
        .map_or(S_OK, |hookdevice| {
            metrics.frames += 1;
            if metrics.frames % 90 == 0 {
                // enforce min fps
                // NOTE: when low, it just sets a boolean flag to disable mod rendering,
                // but we could also use virtual protect to temporarily swap out the hook functions
                // (except for present)
                let now = SystemTime::now();
                let elapsed = now.duration_since(metrics.last_fps_update);
                if let Ok(d) = elapsed {
                    let secs = d.as_secs() as f64 + d.subsec_nanos() as f64 * 1e-9;
                    let fps = metrics.frames as f64 / secs;
                    let smooth_fps = 0.3 * fps + 0.7 * metrics.last_fps;
                    metrics.last_fps = smooth_fps;
                    let min_off = min_fps * 1.1;
                    if smooth_fps < min_fps && !metrics.low_framerate {
                        metrics.low_framerate = true;
                    }
                    // prevent oscillation: don't reactivate until 10% above mininum
                    else if metrics.low_framerate && smooth_fps > (min_off * 1.1) {
                        metrics.low_framerate = false;
                    }
                    // write_log_file(&format!(
                    //     "{} frames in {} secs ({} instant, {} smooth) (low: {})",
                    //     hookdevice.frames, secs, fps, smooth_fps, hookdevice.low_framerate
                    // ));
                    metrics.last_fps_update = now;
                    metrics.frames = 0;
                }
            }
            (hookdevice.real_present)(
                THIS,
                pSourceRect,
                pDestRect,
                hDestWindowOverride,
                pDirtyRegion,
            )
        });

    if GLOBAL_STATE.selection_texture == null_mut() {
        create_selection_texture(THIS);
    }

    if util::appwnd_is_foreground(dev_state().d3d_window) {
        GLOBAL_STATE.input.as_mut().map(|inp| {
            if inp.get_press_fn_count() == 0 {
                setup_input(THIS, inp)
                    .unwrap_or_else(|e| write_log_file(&format!("input setup error: {:?}", e)));
            }
            inp.process()
                .unwrap_or_else(|e| write_log_file(&format!("input error: {:?}", e)));
        });
    }

    if GLOBAL_STATE.is_snapping {
        let now = SystemTime::now();
        let max_dur = std::time::Duration::from_millis(SNAP_MS as u64);
        if now
            .duration_since(GLOBAL_STATE.snap_start)
            .unwrap_or(max_dur)
            >= max_dur
        {
            write_log_file("ending snapshot");
            GLOBAL_STATE.is_snapping = false;
        }
    }

    present_ret
}

pub unsafe extern "system" fn hook_release(THIS: *mut IUnknown) -> ULONG {
    // TODO: hack to work around Release on device while in DIP
    if GLOBAL_STATE.in_hook_release {
        return (dev_state().hook_direct3d9device.as_ref().unwrap().real_release)(THIS);
    }

    GLOBAL_STATE.in_hook_release = true;

    let r = dev_state()
        .hook_direct3d9device
        .as_mut()
        .map_or(0xFFFFFFFF, |hookdevice| {
            hookdevice.ref_count = (hookdevice.real_release)(THIS);

            // if hookdevice.ref_count < 100 {
            //     write_log_file(&format!(
            //         "device {:x} refcount now {}",
            //         THIS as u64, hookdevice.ref_count
            //     ));
            // }

            // could just leak everything on device destroy.  but I know that will
            // come back to haunt me.  so make an effort to purge my stuff when the
            // resource count gets to the expected value, this way the device can be
            // properly disposed.

            let destroying = dev_state().d3d_resource_count > 0
                && hookdevice.ref_count == (dev_state().d3d_resource_count + 1);
            if destroying {
                // purge my stuff
                write_log_file(&format!(
                    "device {:x} refcount is same as internal resource count ({}),
                    it is being destroyed: purging resources",
                    THIS as u64, dev_state().d3d_resource_count
                ));
                purge_device_resources(THIS as *mut IDirect3DDevice9);
                // Note, hookdevice.ref_count is wrong now since we bypassed
                // this function during unload (no re-entrancy).  however the count on the
                // device should be 1 if I did the math right, anyway the release below
                // will fix the count.
            }

            if destroying || (dev_state().d3d_resource_count == 0 && hookdevice.ref_count == 1) {
                // release again to trigger destruction of the device
                hookdevice.ref_count = (hookdevice.real_release)(THIS);
                write_log_file(&format!(
                    "device released: {:x}, refcount: {}",
                    THIS as u64, hookdevice.ref_count
                ));
                if hookdevice.ref_count != 0 {
                    write_log_file(&format!(
                        "WARNING: unexpected ref count of {} after supposedly final
                        device release, device probably leaked",
                        hookdevice.ref_count
                    ));
                }
            }
            hookdevice.ref_count
        });
    GLOBAL_STATE.in_hook_release = false;
    r
}

// TODO: maybe remove if not needed
// pub unsafe extern "system" fn hook_begin_scene(THIS: *mut IDirect3DDevice9) -> HRESULT {
//     if GLOBAL_STATE.in_any_hook_fn() {
//         return (GLOBAL_STATE.hook_direct3d9device.unwrap().real_begin_scene)(THIS);
//     }
//     GLOBAL_STATE.in_beginend_scene = true;

//     if let Err(e) = do_per_frame_operations(THIS) {
//         write_log_file(&format!("unexpected error: {:?}", e));
//         return E_FAIL;
//     }

//     let r = GLOBAL_STATE
//         .hook_direct3d9device
//         .as_ref()
//         .map_or(E_FAIL, |hookdevice| (hookdevice.real_begin_scene)(THIS));

//     GLOBAL_STATE.in_beginend_scene = false;
//     r
// }

decl_profile_globals!(hdip);

pub unsafe extern "system" fn hook_draw_indexed_primitive(
    THIS: *mut IDirect3DDevice9,
    PrimitiveType: D3DPRIMITIVETYPE,
    BaseVertexIndex: INT,
    MinVertexIndex: UINT,
    NumVertices: UINT,
    startIndex: UINT,
    primCount: UINT,
) -> HRESULT {
    let force_modding_off = false;

    profile_start!(hdip, hook_dip);

    // no re-entry please
    profile_start!(hdip, dip_check);
    if GLOBAL_STATE.in_dip {
        write_log_file(&format!("ERROR: i'm in DIP already!"));
        return S_OK;
    }
    profile_end!(hdip, dip_check);

    profile_start!(hdip, state_begin);

    let hookdevice = match dev_state().hook_direct3d9device {
        None => {
            write_log_file(&format!("DIP: No d3d9 device found"));
            return E_FAIL;
        }
        Some(ref mut hookdevice) => hookdevice,
    };
    profile_end!(hdip, state_begin);

    let mut metrics = &mut GLOBAL_STATE.metrics;

    if metrics.low_framerate || !GLOBAL_STATE.show_mods || force_modding_off {
        return (hookdevice.real_draw_indexed_primitive)(
            THIS,
            PrimitiveType,
            BaseVertexIndex,
            MinVertexIndex,
            NumVertices,
            startIndex,
            primCount,
        );
    }

    // snapshotting
    let (override_texture, sel_stage, this_is_selected) = {
        let default = (null_mut(), 0, false);
        if GLOBAL_STATE.making_selection {
            get_selected_texture_stage()
                .map(|stage| {
                    (
                        std::mem::transmute(GLOBAL_STATE.selection_texture),
                        stage,
                        true,
                    )
                })
                .unwrap_or(default)
        } else {
            default
        }
    };

    if this_is_selected && GLOBAL_STATE.is_snapping {
        write_log_file("Snap started");

        (*THIS).AddRef();
        let pre_rc = (*THIS).Release();

        GLOBAL_STATE.device = Some(THIS);

        if GLOBAL_STATE.d3dx_fn.is_none() {
            GLOBAL_STATE.d3dx_fn = d3dx::load_lib(&GLOBAL_STATE.mm_root)
                .map_err(|e| {
                    write_log_file(&format!(
                        "failed to load d3dx: texture snapping not available: {:?}",
                        e
                    ));
                    e
                })
                .ok();
        }

        // TODO: warn about active streams that are in use but not supported
        let mut blending_enabled: DWORD = 0;
        let hr = (*THIS).GetRenderState(D3DRS_INDEXEDVERTEXBLENDENABLE, &mut blending_enabled);
        if hr == 0 && blending_enabled > 0 {
            write_log_file("WARNING: vertex blending is enabled, this mesh may not be supported");
        }

        let mut ok = true;
        let mut vert_decl: *mut IDirect3DVertexDeclaration9 = null_mut();
        // sharpdx does not expose GetVertexDeclaration, so need to do it here
        let hr = (*THIS).GetVertexDeclaration(&mut vert_decl);

        if hr != 0 {
            write_log_file(&format!(
                "Error, can't get vertex declaration.
                Cannot snap; HR: {:x}",
                hr
            ));
        }
        let _vert_decl_rod = ReleaseOnDrop::new(vert_decl);

        ok = ok && hr == 0;
        let mut ib: *mut IDirect3DIndexBuffer9 = null_mut();
        let hr = (*THIS).GetIndices(&mut ib);
        if hr != 0 {
            write_log_file(&format!(
                "Error, can't get index buffer.  Cannot snap; HR: {:x}",
                hr
            ));
        }
        let _ib_rod = ReleaseOnDrop::new(ib);

        ok = ok && hr == 0;

        if ok {
            let mut sd = interop::SnapshotData {
                sd_size: std::mem::size_of::<interop::SnapshotData>() as u32,
                prim_type: PrimitiveType as i32,
                base_vertex_index: BaseVertexIndex,
                min_vertex_index: MinVertexIndex,
                num_vertices: NumVertices,
                start_index: startIndex,
                prim_count: primCount,
                vert_decl: vert_decl,
                index_buffer: ib,
            };
            write_log_file(&format!("snapshot data size is: {}", sd.sd_size));
            GLOBAL_STATE.interop_state.as_mut().map(|is| {
                let cb = is.callbacks;
                let res = (cb.TakeSnapshot)(THIS, &mut sd);
                if res == 0 && snapshot_extra() {
                    let sresult = *(cb.GetSnapshotResult)();
                    let dir = &sresult.directory[0..(sresult.directory_len as usize)];
                    let sprefix = &sresult.snap_file_prefix[0..(sresult.snap_file_prefix_len as usize)];

                    let dir = String::from_utf16(&dir).unwrap_or_else(|_| "".to_owned());
                    let sprefix = String::from_utf16(&sprefix).unwrap_or_else(|_| "".to_owned());
                    // write_log_file(&format!("snap save dir: {}", dir));
                    // write_log_file(&format!("snap prefix: {}", sprefix));
                    constant_tracking::take_snapshot(&dir, &sprefix);
                    shader_capture::take_snapshot(&dir, &sprefix);
                }
            });
        }
        (*THIS).AddRef();
        let post_rc = (*THIS).Release();
        if pre_rc != post_rc {
            write_log_file(&format!(
                "WARNING: device ref count before snapshot ({}) does not
             equal count after snapshot ({}), likely resources were leaked",
                pre_rc, post_rc
            ));
        }

        GLOBAL_STATE.device = None;
    }

    profile_start!(hdip, main_combinator);
    profile_start!(hdip, mod_key_prep);

    GLOBAL_STATE.in_dip = true;

    let mut drew_mod = false;

    // if there is a matching mod, render it
    let modded = 
        GLOBAL_STATE.loaded_mods.as_mut()
        .and_then(|mods| {
            profile_end!(hdip, mod_key_prep);
            profile_start!(hdip, mod_key_lookup);
            let mod_key = NativeModData::mod_key(NumVertices, primCount);
            let r = mods.get(&mod_key);
            // just get out of here if we didn't have a match
            if let None = r {
                profile_end!(hdip, mod_key_lookup);
                return None;
            }
            // found at least one mod.  do some more checks to see if each has a parent, and if the parent
            // is active.  count the active parents we find because if more than one is active, 
            // we have ambiguity and can't render any of them.
            let mut target_mod_index:usize = 0;
            let mut active_parent_name:&str = "";
            let r2 = r.and_then(|nmods| {
                let mut num_active_parents = 0;
                for (midx,nmod) in nmods.iter().enumerate() {
                    if !nmod.parent_mod_name.is_empty() {
                        GLOBAL_STATE.mods_by_name.as_ref() 
                            .and_then(|mbn| mbn.get(&nmod.parent_mod_name))
                            .and_then(|parmodkey| mods.get(parmodkey))
                            .map(|parent_mods| {
                                // count any active parents
                                for parent_mod in parent_mods.iter() {
                                    if num_active_parents > 1 {
                                        // fail, ambiguity
                                        break;
                                    }
                                    if parent_mod.recently_rendered(GLOBAL_STATE.metrics.total_frames) {
                                        // parent is active
                                        num_active_parents += 1;
                                        
                                        // if this parent is for the mod we are looking at, 
                                        // remember that mod index.  not that we'll slam this if we 
                                        // have multiple active parents for multiple mods, 
                                        // but we are screwed anyway in that case.
                                        if nmod.parent_mod_name == parent_mod.name {
                                            active_parent_name = &parent_mod.name;
                                            target_mod_index = midx;
                                        }
                                    }
                                }
                            });
                    } 
                }
                // return Some(()) if we found a valid one.
                // if multiple mods but only one parent, we're good
                if nmods.len() > 1 && num_active_parents == 1 {
                    // write_log_file(&format!("rend mod {} because just one active parent named '{}'", 
                    //     nmods[target_mod_index].name, active_parent_name));
                    Some(())
                }
                // if just one mod it doesn't have a parent, or if it does and there is just one parent,
                // also good.
                else if nmods.len() == 1 && (nmods[0].parent_mod_name.is_empty() || num_active_parents == 1) {
                    // write_log_file(&format!("rend mod {} because just one mod with parname '{}' or {} parents", 
                    // nmods[target_mod_index].name, nmods[0].parent_mod_name, num_active_parents));

                    Some(())
                } else {
                    None
                }
            });
            // return if we aren't rendering it.
            if let None = r2 {
                profile_end!(hdip, mod_key_lookup);
                return None;
            }
            // ok, we're rendering it, but it might be a parent mod too, so we have to set 
            // the last frame on it, which requires a mutable reference.  we couldn't use a 
            // mutable ref earlier, because we had to do two lookups on the hash table.
            // so we have to refetch as mutable, set the frame value and then (for safety)
            // refetch as immutable again so that we can pass that value on.  that's three
            // hash lookups guaranteed but fortunately we're only doing this for active mods.
            // we also can't be clever and return an immutable ref now if it isn't a parent, 
            // because we won't be able to even write the code that checks for the parent 
            // since it would require the get_mut call and thus a mutable and immutable ref 
            // would be active at the same time.
            // TODO: this bullshit could be avoided by using a refcell on the native mods.
            drop(r);
            drop(r2);
            mods.get_mut(&mod_key).map(|nmods| {
                if target_mod_index >= nmods.len() {
                    // error, spam the log i guess
                    write_log_file(&format!("selected target mod index {} exceeds number of mods {}", 
                        target_mod_index, nmods.len()));
                } else {
                    let nmod = &mut nmods[target_mod_index];
                    if nmod.is_parent {
                        nmod.last_frame_render = GLOBAL_STATE.metrics.total_frames;
                    }
                }
            });
            let r = mods.get(&mod_key).and_then(|nmods| {
                if target_mod_index < nmods.len() {
                    Some(&nmods[target_mod_index])
                } else { 
                    None
                }
            });
            profile_end!(hdip, mod_key_lookup);
            r
        })
        .and_then(|nmod| {
            if nmod.mod_data.numbers.mod_type == interop::ModType::Deletion as i32 {
                return Some(nmod.mod_data.numbers.mod_type);
            }
            profile_start!(hdip, mod_render);
            // save state
            let mut pDecl: *mut IDirect3DVertexDeclaration9 = null_mut();
            let ppDecl: *mut *mut IDirect3DVertexDeclaration9 = &mut pDecl;
            let hr = (*THIS).GetVertexDeclaration(ppDecl);
            if hr != 0 {
                write_log_file(&format!(
                    "failed to save vertex declaration when trying to render mod {} {}",
                    NumVertices, primCount
                ));
                return None;
            };

            let mut pStreamVB: *mut IDirect3DVertexBuffer9 = null_mut();
            let ppStreamVB: *mut *mut IDirect3DVertexBuffer9 = &mut pStreamVB;
            let mut offsetBytes: UINT = 0;
            let mut stride: UINT = 0;

            let hr = (*THIS).GetStreamSource(0, ppStreamVB, &mut offsetBytes, &mut stride);
            if hr != 0 {
                write_log_file(&format!(
                    "failed to save vertex data when trying to render mod {} {}",
                    NumVertices, primCount
                ));
                if pDecl != null_mut() {
                    (*pDecl).Release();
                }
                return None;
            }

            // Note: C++ code did not change StreamSourceFreq...may need it for some games.
            // draw override
            (*THIS).SetVertexDeclaration(nmod.decl);
            (*THIS).SetStreamSource(0, nmod.vb, 0, nmod.mod_data.numbers.vert_size_bytes as u32);

            // and set mod textures
            let mut save_tex:[Option<*mut IDirect3DBaseTexture9>; 4] = [None; 4];
            let mut _st_rods:Vec<ReleaseOnDrop<*mut IDirect3DBaseTexture9>> = vec![];
            for (i,tex) in nmod.textures.iter().enumerate() {
                if *tex != null_mut() {
                    //write_log_file(&format!("set override tex stage {} to {:x} for mod {}/{}", i, *tex as u64, NumVertices, primCount));
                    let mut save:*mut IDirect3DBaseTexture9 = null_mut();
                    (*THIS).GetTexture(i as u32, &mut save);
                    save_tex[i] = Some(save);
                    (*THIS).SetTexture(i as u32, *tex as *mut IDirect3DBaseTexture9);
                    _st_rods.push(ReleaseOnDrop::new(save));
                }
            }
            
            // set the override tex, which is the (usually) the selection tex.  this might overwrite
            // the mod tex tex we just set.
            let mut save_texture: *mut IDirect3DBaseTexture9 = null_mut();
            let _st_rod = {
                if override_texture != null_mut() {
                    (*THIS).GetTexture(sel_stage, &mut save_texture);
                    (*THIS).SetTexture(sel_stage, override_texture);
                    Some(ReleaseOnDrop::new(save_texture))
                } else {
                    None
                }
            };

            (*THIS).DrawPrimitive(
                nmod.mod_data.numbers.prim_type as u32,
                0,
                nmod.mod_data.numbers.prim_count as u32,
            );
            drew_mod = true;

            // restore state
            (*THIS).SetVertexDeclaration(pDecl);
            (*THIS).SetStreamSource(0, pStreamVB, offsetBytes, stride);
            // restore textures
            for (i,tex) in save_tex.iter().enumerate() {
                tex.map(|tex| {
                    (*THIS).SetTexture(i as u32, tex);
                });
            }
            if override_texture != null_mut() {
                (*THIS).SetTexture(sel_stage, save_texture);
            }
            (*pDecl).Release();
            (*pStreamVB).Release();
            profile_end!(hdip, mod_render);

            Some(nmod.mod_data.numbers.mod_type)
        });
    profile_end!(hdip, main_combinator);

    profile_start!(hdip, draw_input_check);
    // draw input if not modded or if mod is additive
    let draw_input = match modded {
        None => true,
        Some(mtype) if interop::ModType::CPUAdditive as i32 == mtype => true,
        Some(_) => false,
    };
    profile_end!(hdip, draw_input_check);

    profile_start!(hdip, real_dip);
    let dresult = if draw_input {
        let mut save_texture: *mut IDirect3DBaseTexture9 = null_mut();
        let _st_rod = {
            if override_texture != null_mut() {
                (*THIS).GetTexture(sel_stage, &mut save_texture);
                (*THIS).SetTexture(sel_stage, override_texture);
                Some(ReleaseOnDrop::new(save_texture))
            } else {
                None
            }
        };
        let r = (hookdevice.real_draw_indexed_primitive)(
            THIS,
            PrimitiveType,
            BaseVertexIndex,
            MinVertexIndex,
            NumVertices,
            startIndex,
            primCount,
        );
        if override_texture != null_mut() {
            (*THIS).SetTexture(sel_stage, save_texture);
        }
        r
    } else {
        S_OK
    };
    profile_end!(hdip, real_dip);

    profile_start!(hdip, statistics);
    // statistics
    metrics.dip_calls += 1;
    if metrics.dip_calls % 500_000 == 0 {
        let now = SystemTime::now();
        let elapsed = now.duration_since(metrics.last_call_log);
        match elapsed {
            Ok(d) => {
                let secs = d.as_secs() as f64 + d.subsec_nanos() as f64 * 1e-9;
                if secs >= 10.0 {
                    let dipsec = metrics.dip_calls as f64 / secs;

                    let epocht = now
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or(std::time::Duration::from_secs(1))
                        .as_secs()
                        * 1000;

                    write_log_file(&format!(
                        "{:?}: {} dip calls in {:.*} secs ({:.*} dips/sec (fps: {:.*}))",
                        epocht, metrics.dip_calls, 2, secs, 2, dipsec, 2, metrics.last_fps
                    ));
                    GLOBAL_STATE.active_texture_set.as_ref().map(|set| {
                        write_log_file(&format!(
                            "active texture set contains: {} textures",
                            set.len()
                        ))
                    });
                    metrics.last_call_log = now;
                    metrics.dip_calls = 0;
                }
            }
            Err(e) => write_log_file(&format!("Error getting elapsed duration: {:?}", e)),
        }
    }
    profile_end!(hdip, statistics);

    GLOBAL_STATE.in_dip = false;
    profile_end!(hdip, hook_dip);

    profile_summarize!(hdip);

    dresult
}

// =====================
// Everything after this should be moved into device lib

unsafe fn hook_device(
    device: *mut IDirect3DDevice9,
    _guard: &std::sync::MutexGuard<()>,
) -> Result<HookDirect3D9Device> {
    //write_log_file(&format!("gs hook_direct3d9device is some: {}", GLOBAL_STATE.hook_direct3d9device.is_some()));
    write_log_file(&format!("hooking new device: {:x}", device as u64));
    // Oddity: each device seems to have its own vtbl.  So need to hook each one of them.
    // but the direct3d9 instance seems to share a vtbl between different instances.  So need to only
    // hook those once.  I'm not sure why this is.
    let vtbl: *mut IDirect3DDevice9Vtbl = std::mem::transmute((*device).lpVtbl);
    write_log_file(&format!("device vtbl: {:x}", vtbl as u64));
    let vsize = std::mem::size_of::<IDirect3DDevice9Vtbl>();

    let real_draw_indexed_primitive = (*vtbl).DrawIndexedPrimitive;
    //let real_begin_scene = (*vtbl).BeginScene;
    let real_release = (*vtbl).parent.Release;
    let real_present = (*vtbl).Present;

    // remember these functions but don't hook them yet
    let real_set_texture = (*vtbl).SetTexture;

    let real_set_vertex_sc_f = (*vtbl).SetVertexShaderConstantF;
    let real_set_vertex_sc_i = (*vtbl).SetVertexShaderConstantI;
    let real_set_vertex_sc_b = (*vtbl).SetVertexShaderConstantB;

    let real_set_pixel_sc_f = (*vtbl).SetPixelShaderConstantF;
    let real_set_pixel_sc_i = (*vtbl).SetPixelShaderConstantI;
    let real_set_pixel_sc_b = (*vtbl).SetPixelShaderConstantB;

    let old_prot = unprotect_memory(vtbl as *mut c_void, vsize)?;

    (*vtbl).DrawIndexedPrimitive = hook_draw_indexed_primitive;
    //(*vtbl).BeginScene = hook_begin_scene;
    (*vtbl).Present = hook_present;
    (*vtbl).parent.Release = hook_release;

    protect_memory(vtbl as *mut c_void, vsize, old_prot)?;

    // Inc ref count on the device
    (*device).AddRef();

    // shader constants init
    if constant_tracking::is_enabled() {
        GLOBAL_STATE.vertex_constants = Some(constant_tracking::ConstantGroup::new());
        GLOBAL_STATE.pixel_constants = Some(constant_tracking::ConstantGroup::new());

        (*vtbl).SetVertexShaderConstantF = constant_tracking::hook_set_vertex_sc_f;
        (*vtbl).SetVertexShaderConstantI = constant_tracking::hook_set_vertex_sc_i;
        (*vtbl).SetVertexShaderConstantB = constant_tracking::hook_set_vertex_sc_b;

        (*vtbl).SetPixelShaderConstantF = constant_tracking::hook_set_pixel_sc_f;
        (*vtbl).SetPixelShaderConstantI = constant_tracking::hook_set_pixel_sc_i;
        (*vtbl).SetPixelShaderConstantB = constant_tracking::hook_set_pixel_sc_b;
    }
    write_log_file(&format!("constant tracking enabled: {}", constant_tracking::is_enabled()));

    Ok(HookDirect3D9Device::new(
        real_draw_indexed_primitive,
        //real_begin_scene,
        real_present,
        real_release,
        real_set_texture,
        real_set_vertex_sc_f,
        real_set_vertex_sc_i,
        real_set_vertex_sc_b,
        real_set_pixel_sc_f,
        real_set_pixel_sc_i,
        real_set_pixel_sc_b,
    ))
}

#[inline]
unsafe fn create_and_hook_device(
    THIS: *mut IDirect3D9,
    Adapter: UINT,
    DeviceType: D3DDEVTYPE,
    hFocusWindow: HWND,
    BehaviorFlags: DWORD,
    pPresentationParameters: *mut D3DPRESENT_PARAMETERS,
    ppReturnedDeviceInterface: *mut *mut IDirect3DDevice9,
) -> Result<()> {
    let lock = GLOBAL_STATE_LOCK
        .lock()
        .map_err(|_err| HookError::GlobalLockError)?;

    if DEVICE_STATE == null_mut() {
        return Err(HookError::BadStateError("no device state pointer??".to_owned()));
    }
    (*DEVICE_STATE)
        .hook_direct3d9
        .as_mut()
        .ok_or(HookError::Direct3D9InstanceNotFound)
        .and_then(|hd3d9| {
            write_log_file(&format!("calling real create device"));
            if BehaviorFlags & D3DCREATE_MULTITHREADED == D3DCREATE_MULTITHREADED {
                write_log_file(&format!(
                    "Notice: device being created with D3DCREATE_MULTITHREADED"
                ));
            }
            let result = (hd3d9.real_create_device)(
                THIS,
                Adapter,
                DeviceType,
                hFocusWindow,
                BehaviorFlags,
                pPresentationParameters,
                ppReturnedDeviceInterface,
            );
            if result != S_OK {
                write_log_file(&format!("create device FAILED: {}", result));
                return Err(HookError::CreateDeviceFailed(result));
            }
            (*DEVICE_STATE).d3d_window = hFocusWindow;
            hook_device(*ppReturnedDeviceInterface, &lock)
        })
        .and_then(|hook_d3d9device| {
            (*DEVICE_STATE).hook_direct3d9device = Some(hook_d3d9device);
            write_log_file(&format!(
                "hooked device on thread {:?}",
                std::thread::current().id()
            ));
            Ok(())
        })
        .or_else(|err| {
            if ppReturnedDeviceInterface != null_mut() && *ppReturnedDeviceInterface != null_mut() {
                (*(*ppReturnedDeviceInterface)).Release();
            }
            Err(err)
        })
}

pub unsafe extern "system" fn hook_create_device(
    THIS: *mut IDirect3D9,
    Adapter: UINT,
    DeviceType: D3DDEVTYPE,
    hFocusWindow: HWND,
    BehaviorFlags: DWORD,
    pPresentationParameters: *mut D3DPRESENT_PARAMETERS,
    ppReturnedDeviceInterface: *mut *mut IDirect3DDevice9,
) -> HRESULT {
    let res = create_and_hook_device(
        THIS,
        Adapter,
        DeviceType,
        hFocusWindow,
        BehaviorFlags,
        pPresentationParameters,
        ppReturnedDeviceInterface,
    );

    // create input, but don't fail everything if we can't (may be able to still use read-only mode)
    input::Input::new()
        .map(|inp| {
            GLOBAL_STATE.input = Some(inp);
        })
        .unwrap_or_else(|e| {
            write_log_file(&format!(
                "failed to create input; only playback from existing mods will be possible: {:?}",
                e
            ))
        });

    match res {
        Err(e) => {
            write_log_file(&format!("error creating/hooking device: {:?}", e));
            E_FAIL
        }
        Ok(_) => S_OK,
    }
}

// perf event typedefs from:
// https://github.com/Microsoft/DXUT/blob/942a9f4e30abf6d5d0c1b3529c17cd6b574743f9/Core/DXUTmisc.cpp
#[allow(unused)]
#[no_mangle]
// typedef INT         (WINAPI * LPD3DPERF_BEGINEVENT)(DWORD, LPCWSTR);
pub extern "system" fn D3DPERF_BeginEvent(a: DWORD, b: LPCWSTR) -> i32 {
    0
}
#[allow(unused)]
#[no_mangle]
// typedef INT         (WINAPI * LPD3DPERF_ENDEVENT)(void);
pub extern "system" fn D3DPERF_EndEvent() -> i32 {
    0
}
#[allow(unused)]
#[no_mangle]
// typedef VOID        (WINAPI * LPD3DPERF_SETMARKER)(DWORD, LPCWSTR);
pub extern "system" fn D3DPERF_SetMarker(a: DWORD, b: LPCWSTR) -> () {}
#[allow(unused)]
#[no_mangle]
// typedef VOID        (WINAPI * LPD3DPERF_SETREGION)(DWORD, LPCWSTR);
pub extern "system" fn D3DPERF_SetRegion(a: DWORD, b: LPCWSTR) -> () {}
#[allow(unused)]
#[no_mangle]
// typedef BOOL        (WINAPI * LPD3DPERF_QUERYREPEATFRAME)(void);
pub extern "system" fn D3DPERF_QueryRepeatFrame() -> BOOL {
    FALSE
}
#[allow(unused)]
#[no_mangle]
// typedef VOID        (WINAPI * LPD3DPERF_SETOPTIONS)( DWORD dwOptions );
pub extern "system" fn D3DPERF_SetOptions(ops: DWORD) -> () {}
#[allow(unused)]
#[no_mangle]
// typedef DWORD (WINAPI * LPD3DPERF_GETSTATUS)();
pub extern "system" fn D3DPERF_GetStatus() -> DWORD {
    0
}

type Direct3DCreate9Fn = unsafe extern "system" fn(sdk_ver: u32) -> *mut IDirect3D9;

#[allow(unused)]
#[no_mangle]
pub extern "system" fn Direct3DCreate9(SDKVersion: u32) -> *mut u64 {
    match create_d3d9(SDKVersion) {
        Ok(ptr) => ptr as *mut u64,
        Err(x) => {
            write_log_file(&format!("create_d3d failed: {:?}", x));
            std::ptr::null_mut()
        }
    }
}

pub fn create_d3d9(sdk_ver: u32) -> Result<*mut IDirect3D9> {
    unsafe {
        if DEVICE_STATE == null_mut() {
            DEVICE_STATE = Box::into_raw(Box::new(DeviceState {
                hook_direct3d9: None,
                hook_direct3d9device: None,
                d3d_window: null_mut(),
                d3d_resource_count: 0,
            }));
        }
    };

    let handle = util::load_lib("c:\\windows\\system32\\d3d9.dll")?; // Todo: use GetSystemDirectory
    let addr = util::get_proc_address(handle, "Direct3DCreate9")?;

    let make_it = || unsafe {
        let create: Direct3DCreate9Fn = std::mem::transmute(addr);

        let direct3d9 = (create)(sdk_ver);
        let direct3d9 = direct3d9 as *mut IDirect3D9;
        direct3d9
    };

    unsafe {
        let mm_root = match get_mm_conf_info() {
            Ok((true, Some(dir))) => dir,
            Ok((false, _)) => {
                write_log_file(&format!("ModelMod not initializing because it is not active (did you start it with the ModelMod launcher?)"));
                return Ok(make_it());
            }
            Ok((true, None)) => {
                write_log_file(&format!("ModelMod not initializing because install dir not found (did you start it with the ModelMod launcher?)"));
                return Ok(make_it());
            }
            Err(e) => {
                write_log_file(&format!(
                    "ModelMod not initializing due to conf error: {:?}",
                    e
                ));
                return Ok(make_it());
            }
        };

        // try to create log file using module name and root dir.  if it fails then just
        // let logging go to the temp dir file.
        get_module_name()
            .and_then(|mod_name| {
                use std::path::PathBuf;

                let stem = {
                    let pb = PathBuf::from(&mod_name);
                    let s = pb
                        .file_stem()
                        .ok_or(HookError::ConfReadFailed("no stem".to_owned()))?;
                    let s = s
                        .to_str()
                        .ok_or(HookError::ConfReadFailed("cant't make stem".to_owned()))?;
                    (*s).to_owned()
                };

                let file_name = format!("ModelMod.{}.log", stem);

                let mut tdir = mm_root.to_owned();
                tdir.push_str("\\Logs\\");
                let mut tname = tdir.to_owned();
                tname.push_str(&file_name);

                use std::fs::OpenOptions;
                use std::io::Write;
                // don't open append first time so that log is cleared.
                let mut f = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tname)?;
                writeln!(f, "ModelMod initialized\r")?;

                // if that succeeded then we can set the file name now
                set_log_file_path(&tdir, &file_name)?;

                eprintln!("Log File: {}", tname);

                Ok(())
            })
            .map_err(|e| {
                write_log_file(&format!("error setting custom log file name: {:?}", e));
            })
            .unwrap_or(());

        let direct3d9 = make_it();
        write_log_file(&format!("created d3d: {:x}", direct3d9 as u64));

        // let vtbl: *mut IDirect3D9Vtbl = std::mem::transmute((*direct3d9).lpVtbl);
        // write_log_file(&format!("vtbl: {:x}", vtbl as u64));

        // don't hook more than once
        let _lock = GLOBAL_STATE_LOCK
            .lock()
            .map_err(|_err| HookError::D3D9HookFailed)?;

        if (*DEVICE_STATE).hook_direct3d9.is_some() {
            return Ok(direct3d9);
        }

        GLOBAL_STATE.mm_root = Some(mm_root);

        // get pointer to original vtable
        let vtbl: *mut IDirect3D9Vtbl = std::mem::transmute((*direct3d9).lpVtbl);

        // save pointer to real function
        let real_create_device = (*vtbl).CreateDevice;
        // write_log_file(&format!(
        //     "hooking real create device, hookfn: {:?}, realfn: {:?} ",
        //     hook_create_device as u64, real_create_device as u64
        // ));

        // unprotect memory and slam the vtable
        let vsize = std::mem::size_of::<IDirect3D9Vtbl>();
        let old_prot = util::unprotect_memory(vtbl as *mut c_void, vsize)?;

        (*vtbl).CreateDevice = hook_create_device;

        util::protect_memory(vtbl as *mut c_void, vsize, old_prot)?;

        // create hookstate
        let hd3d9 = HookDirect3D9 {
            real_create_device: real_create_device,
        };

        (*DEVICE_STATE).hook_direct3d9 = Some(hd3d9);

        Ok(direct3d9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate test;

    use test::*;

    #[allow(unused)]
    pub unsafe extern "system" fn stub_draw_indexed_primitive(
        THIS: *mut IDirect3DDevice9,
        arg1: D3DPRIMITIVETYPE,
        BaseVertexIndex: INT,
        MinVertexIndex: UINT,
        NumVertices: UINT,
        startIndex: UINT,
        primCount: UINT,
    ) -> HRESULT {
        test::black_box(());
        S_OK
    }

    #[allow(unused)]
    pub unsafe extern "system" fn stub_begin_scene(THIS: *mut IDirect3DDevice9) -> HRESULT {
        test::black_box(());
        S_OK
    }

    #[allow(unused)]
    pub unsafe extern "system" fn stub_release(THIS: *mut IUnknown) -> ULONG {
        test::black_box(());
        0
    }

    #[allow(unused)]
    unsafe extern "system" fn stub_present(
        THIS: *mut IDirect3DDevice9,
        pSourceRect: *const RECT,
        pDestRect: *const RECT,
        hDestWindowOverride: HWND,
        pDirtyRegion: *const RGNDATA,
    ) -> HRESULT {
        test::black_box(());
        0
    }

    fn set_stub_device() {
        // let d3d9device = HookDirect3D9Device::new(
        //     stub_draw_indexed_primitive,
        //     stub_begin_scene,
        //     stub_present,
        //     stub_release);
        // set_hook_device(d3d9device);
    }

    #[test]
    fn can_create_d3d9() {
        //use test_e2e;
        // TODO: need to figure out why this behaves poorly WRT test_e2e.
        // is it a side effect of rust's threaded test framework or a system of issues.

        // let _lock = test_e2e::TEST_MUTEX.lock().unwrap();
        // let d3d9 = create_d3d9(32);
        // if let &Err(ref x) = &d3d9 {
        //     assert!(false, format!("unable to create d39: {:?}", x));
        // }
        // unsafe { d3d9.map(|d3d9| (*d3d9).Release()) };
        // let d3d9 = create_d3d9(32);
        // if let &Err(ref x) = &d3d9 {
        //     assert!(false, format!("unable to create d39: {:?}", x));
        // }
        // unsafe { d3d9.map(|d3d9| (*d3d9).Release()) };
        // println!("=============== exiting");
    }
    #[test]
    fn test_state_copy() {
        //set_stub_device();

        // TODO: re-enable when per-scene ops creates clr in global state
        // unsafe {
        //     let device = std::ptr::null_mut();
        //     hook_begin_scene(device);
        //     for _i in 0..10 {
        //         hook_draw_indexed_primitive(device, D3DPT_TRIANGLESTRIP, 0, 0, 0, 0, 0);
        //     }
        // };
    }

    #[bench]
    fn dip_call_time(_b: &mut Bencher) {
        //set_stub_device();

        // Core-i7-6700 3.4Ghz, 1.25 nightly 2018-01-13
        // 878600000 dip calls in 10.0006051 secs (87854683.91307643 dips/sec)
        // 111,695,214 ns/iter (+/- 2,909,577)
        // ~88K calls/millisecond

        // TODO: re-enable when per-scene ops creates clr in global state

        // let device = std::ptr::null_mut();
        // unsafe { hook_begin_scene(device) };
        // b.iter(|| {
        //     let range = 0..10_000_000;
        //     for _r in range {
        //         unsafe { hook_draw_indexed_primitive(device,
        //             D3DPT_TRIANGLESTRIP, 0, 0, 0, 0, 0) };
        //     }
        // });
    }
}
