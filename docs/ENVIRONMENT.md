# Launch options and environment variables

`python tuxbox.py run <game>` needs no variables for a normal launch. This page lists what you can change.

## Launch options

Place options before the game path. Anything after the path goes to the game.

| Option | Default | Effect |
| --- | --- | --- |
| `--app-id ID` | none | Select a manifest entry by its Id |
| `--backend` | `proton` on Linux, `native` on Windows | `native`, `proton` or `wine` |
| `--proton PATH` | the pinned GE-Proton11-14 | Use another Proton distribution |
| `--prefix PATH` | `~/.local/share/project-tuxbox/prefixes/<proton>` | Wine prefix |
| `--offline` | off, always on for ProjectScarlett | Launch without network access (Linux) |
| `--log-file PATH` | off | Also append the game log to this file |
| `--mangohud` | off | Show and record frame times (Linux) |
| `--profile-pipelines` | off | Record pipeline creation times |
| `--prepare-only` | off | Prepare shader caches and exit |
| `--no-shader-prep` | off | Skip launch-time shader preparation |
| `--graphics-menu`, `--graphics-profile` | off | ProjectScarlett graphics trials |

## Set by the launcher

The launcher sets these for a Proton or Wine launch. A value you set yourself is kept, except where noted.

| Variable | Value | Purpose |
| --- | --- | --- |
| `VKD3D_SHADER_CACHE_PATH` | `<prefix>/shadercache` | vkd3d-proton's pipeline cache; `0` disables it |
| `__GL_SHADER_DISK_CACHE` | `1` | NVIDIA driver cache; `0` leaves it off |
| `__GL_SHADER_DISK_CACHE_PATH` | `<prefix>/shadercache` | Gives each prefix its own NVIDIA cache |
| `__GL_SHADER_DISK_CACHE_SKIP_CLEANUP` | `1` | Asks the NVIDIA driver not to trim the cache |
| `__GL_SHADER_DISK_CACHE_SIZE` | 32 GiB | The limit if the driver trims anyway |
| `TUXBOX_LOG` | `1` (`stderr` on Windows) | Sends provider messages to the game log |
| `WINEDEBUG`, `VKD3D_DEBUG`, `DXVK_LOG_LEVEL` | warnings and errors | Backend messages in the game log |
| `UMU_RUNTIME_UPDATE` | `0` for offline launches (always set) | Stops umu-run from needing the network |

## Behaviour

| Variable | Default | Effect |
| --- | --- | --- |
| `TUXBOX_RUNTIME_SHADERS` | on | `0` stops translating shaders during play |
| `TUXBOX_RUNTIME_SHADER_JOBS` | half the CPU cores, at least 2 where there are two, at most 32 | Compiler jobs during play, 1 to 32 |
| `TUXBOX_SHADER_PREP_JOBS` | one per CPU core, at most 32 | Compiler jobs for ProjectScarlett preparation |
| `TUXBOX_SHADER_PREP` | `chase` | Frosty preparation: `0`, `1`, `menu` or `chase` |
| `TUXBOX_SHADER_PREP_UI` | on for ProjectScarlett | `1` shows a desktop progress window |
| `TUXBOX_SCARLETT_SEED_PACK` | off | `1` prepares every shader in `shaders.rpf` |
| `TUXBOX_LARGE_COMPILES` | `4` | Large compute pipelines compiling at once; `0` removes the limit |
| `TUXBOX_PIPELINE_DEADLINE` | `2` | Seconds pipeline creation waits for the driver before the compile continues in the background; `0` waits |
| `TUXBOX_PIPELINE_MAX_LIVE` | one per CPU core, 2 to 16 | Background pipeline compiles at once, 1 to 16 |
| `TUXBOX_PIPELINE_MAX_QUEUED` | `64` | Background compiles waiting for a slot, 1 to 4096; past it pipeline creation waits |
| `TUXBOX_MESH_COMPILES` | off | Native mesh pipelines created at once |
| `TUXBOX_NULL_PIPELINES` | on | `0` fails unsupported pipelines instead of giving empty stand-ins |
| `TUXBOX_PROGRESS_OVERLAY` | `auto` | `always` or `0` for the progress panel |
| `TUXBOX_PROGRESS_OVERLAY_POS` | `tr` | `tl`, `tr`, `bl` or `br` |
| `TUXBOX_KEYBOARD_GAMEPAD` | on | `0` leaves only physical XInput controllers |
| `TUXBOX_STORAGE_THREADS` | CPU cores, at most 8 | Storage request threads |
| `TUXBOX_MANGOHUD_PCI` | chosen automatically | PCI address of the GPU MangoHud reports |

## Testing another path

| Variable | Effect |
| --- | --- |
| `TUXBOX_HOST_PROFILE` | Replace what the [graphics driver reports](USAGE.md#host-profile) |
| `TUXBOX_PERSISTENT_DXC` | Path of a persistent DXC helper to use during play |
| `TUXBOX_REFERENCE_CBV` | `1` applies the constant-buffer layer to a provider TuxBox didn't build |
| `TUXBOX_RASTER_ROOT_FALLBACK` | Try an unsupported raster root signature unchanged |
| `TUXBOX_DESCRIPTOR_HEAP_LIMIT` | Cap shader-visible descriptor heaps to force paging |
| `TUXBOX_REQUIRE_BUFFER_IMPORT` | `1` fails instead of falling back when memory can't be imported |
| `TUXBOX_RUNTIME_SERVICES` | `0`, or a list such as `XUser,XGame`, limits the Xbox runtime services |
| `TUXBOX_RUNTIME_PROBE` | `1` also exposes the unidentified runtime class |
| `TUXBOX_PACKAGED` | `1` reports a packaged process |
| `TUXBOX_SURVEY` | `1` continues past recoverable failures to report several in one launch |

## Diagnostics

All are off by default. Capture variables write the game's data to disk, so don't share the output.

| Variable | Reports or saves |
| --- | --- |
| `PROTON_LOG`, `PROTON_LOG_DIR` | Proton's own log |
| `TUXBOX_TRACE_SHADERS` | Shader cache lookups |
| `TUXBOX_TRACE_PRESENT` | Presented frames and writes to display resources |
| `TUXBOX_TRACE_RASTER_DRAWS`, `TUXBOX_TRACE_RASTER_ROOTS`, `TUXBOX_TRACE_RASTER_INPUTS` | Draws, their roots and inputs |
| `TUXBOX_TRACE_RAYTRACING` | Acceleration structures and ray pipelines |
| `TUXBOX_TRACE_STENCIL` | Stencil export routing |
| `TUXBOX_TRACE_HEAP_IMPORTS`, `TUXBOX_TRACE_BUFFER_BACKING` | How guest memory reaches the GPU |
| `TUXBOX_TRACE_ALLOCATION_BYTES` | Allocations of the listed sizes |
| `TUXBOX_WATCH_ADDRESS` | Allocations and resources near `<hex>[,<hex radius>]` |
| `TUXBOX_XG_TRACE` | XG address calculations |
| `TUXBOX_WAVE_DEBUG` | Why the translator didn't mask a wave reduction |
| `TUXBOX_CAPTURE_SHADER_INPUTS`, `TUXBOX_CAPTURE_RASTER_ROOTS`, `TUXBOX_CAPTURE_RAY_ROOTS` | Folder for shader inputs and root data |
| `TUXBOX_CAPTURE_BLAS` | Folder for acceleration structure inputs |
| `TUXBOX_CAPTURE_RASTER_PAIR`, `TUXBOX_CAPTURE_DRAW_READBACK` | Selected draws and their output |

## Build and test tools

`TUXBOX_XSHADER` selects the shader compiler for launcher preparation and `DXC` selects the DirectX Shader Compiler. `TUXBOX_BLESS_PROFILES`, `TUXBOX_ORACLE_*`, `TUXBOX_TEST_*` and the probe and replay variables in `tools/` belong to individual checks. `TUXBOX_PROVIDER`, `TUXBOX_SHADER_SESSION` and `TUXBOX_WINE_*` are internal; don't set them.
