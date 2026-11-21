<#
Launches the debug build off screen against local test servers, plays a
script of actions and saves captures of the window. Nothing appears on the
desktop and the focus is never taken, so it is safe to run while the machine
is in use.

    # one-off: local servers for the three protocols
    docker run -d --name s3v-test -p 127.0.0.1:9000:9000 `
        -e RUSTFS_ACCESS_KEY=devaccess -e RUSTFS_SECRET_KEY=devsecret123 rustfs/rustfs /data
    docker run -d --name s3v-ssh -p 127.0.0.1:2222:2222 -e PUID=1000 -e PGID=1000 `
        -e PASSWORD_ACCESS=true -e USER_NAME=dev -e USER_PASSWORD=devpass123 linuxserver/openssh-server
    docker run -d --name s3v-ftp -p 127.0.0.1:2121:21 -p 127.0.0.1:21100-21110:21100-21110 `
        -e "USERS=dev|devpass123" -e ADDRESS=127.0.0.1 -e MIN_PORT=21100 -e MAX_PORT=21110 `
        delfer/alpine-ftp-server

    cargo build
    .\scripts\dev-run.ps1 -Script "connect 1; panes both; send ls -la" -At 4, 6

The connections are numbered from 0 in the order given; the first one is
opened at startup. Steps run about 0.8 s apart, starting roughly 2.5 s after
launch. They are listed in `App::run_dev_step` (src/app.rs); the variables
are in src/dev.rs. Posting mouse messages to the window does not work for
this: Windows reports the cursor as gone before the click lands. Use the
script steps instead.

To work on the list of connections without the real one, and without any
server, show a fictional list instead (see scripts/fixtures/hosts.json):

    .\scripts\dev-run.ps1 -Profiles scripts\fixtures\hosts.json -Connections @() `
        -Script "key ctrl+k; text prep; key down; key enter" -At 4, 6

The same servers back the live tests:

    $env:S3V_TEST_SFTP = "dev:devpass123@127.0.0.1:2222"
    $env:S3V_TEST_FTP = "dev:devpass123@127.0.0.1:2121"
    $env:S3V_TEST_ENDPOINT = "http://127.0.0.1:9000"
    $env:S3V_TEST_ACCESS_KEY = "devaccess"; $env:S3V_TEST_SECRET_KEY = "devsecret123"
    cargo test
#>
param(
    [string]$Script = "",
    [string[]]$Connections = @(
        "Stockage S3=http://devaccess:devsecret123@127.0.0.1:9000",
        "Serveur SSH=sftp://dev:devpass123@127.0.0.1:2222",
        "Serveur FTP=ftp://dev:devpass123@127.0.0.1:2121"
    ),
    [double[]]$At = @(4),
    [string]$Name = "capture",
    [string]$OutDir = (Join-Path $env:TEMP "s3viewer-dev"),
    [string]$UploadSource = "",
    # Folder holding sitemanager.xml, recentservers.xml, settings.json and
    # ssh_config for the `import` step. The real files are never read here.
    [string]$ImportDir = "",
    # JSON file of connections (the format of profiles.json) shown in place
    # of the saved ones, for runs that must not depend on the real list.
    # Combine with -Connections @() to open nothing at startup.
    [string]$Profiles = "",
    [switch]$Release,
    # Another build to run, such as a release built apart for measures
    # (`$env:CARGO_TARGET_DIR="target/perf"; cargo build --release`), so that
    # target\release, which the real app may be running from, is left alone.
    [string]$Exe = "",
    [switch]$Keep
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class DevWindow {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct ENTRY {
        public uint size, usage, pid; public IntPtr heap; public uint module, threads, parent; public int priority; public uint flags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string exe;
    }
    [DllImport("kernel32.dll")] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern bool Process32FirstW(IntPtr s, ref ENTRY e);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern bool Process32NextW(IntPtr s, ref ENTRY e);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    // The process and everything it started, at any depth.
    public static HashSet<uint> Family(uint root) {
        var parents = new List<KeyValuePair<uint, uint>>();
        var snapshot = CreateToolhelp32Snapshot(2, 0);
        var entry = new ENTRY(); entry.size = (uint)Marshal.SizeOf(typeof(ENTRY));
        for (var more = Process32FirstW(snapshot, ref entry); more; more = Process32NextW(snapshot, ref entry)) {
            parents.Add(new KeyValuePair<uint, uint>(entry.pid, entry.parent));
        }
        CloseHandle(snapshot);
        var family = new HashSet<uint> { root };
        for (var grew = true; grew; ) {
            grew = false;
            foreach (var pair in parents) { if (family.Contains(pair.Value) && family.Add(pair.Key)) { grew = true; } }
        }
        return family;
    }
    // Visible top-level windows of these processes, but `except`.
    public static List<string> Visible(HashSet<uint> wanted, IntPtr except) {
        var found = new List<string>();
        EnumWindows((h, l) => {
            uint pid;
            GetWindowThreadProcessId(h, out pid);
            RECT r;
            var name = new StringBuilder(256);
            GetClassName(h, name, 256);
            // winit's message window is layered and never painted: it shows nothing.
            if (h != except && wanted.Contains(pid) && IsWindowVisible(h) && GetWindowRect(h, out r)
                && r.R > r.L && r.B > r.T && name.ToString() != "Winit Thread Event Target") {
                found.Add(pid + " " + name + " " + (r.R - r.L) + "x" + (r.B - r.T) + " at " + r.L + "," + r.T);
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
"@
[DevWindow]::SetProcessDPIAware() | Out-Null

# Asks the window for its own pixels: works off screen and behind other windows.
function Save-WindowCapture([int]$ProcessId, [string]$File) {
    $process = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if (-not $process -or $process.MainWindowHandle -eq 0) { Write-Warning "no window to capture"; return }
    $rect = New-Object DevWindow+RECT
    [DevWindow]::GetWindowRect($process.MainWindowHandle, [ref]$rect) | Out-Null
    $bitmap = New-Object System.Drawing.Bitmap ($rect.R - $rect.L), ($rect.B - $rect.T)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $hdc = $graphics.GetHdc()
    [DevWindow]::PrintWindow($process.MainWindowHandle, $hdc, 2) | Out-Null
    $graphics.ReleaseHdc($hdc)
    $bitmap.Save($File, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose(); $bitmap.Dispose()
    Write-Output $File
}

$profile = if ($Release) { "release" } else { "debug" }
$program = Join-Path (Split-Path -Parent $PSScriptRoot) "target\$profile\s3viewer.exe"
# (PowerShell names are case-insensitive: `$program`, not `$exe`, which is `$Exe`.)
if ($Exe) { $program = (Resolve-Path $Exe).Path }
if (-not (Test-Path $program)) { throw "build first: cargo build" }
New-Item -ItemType Directory -Force $OutDir, (Join-Path $OutDir "downloads") | Out-Null

$env:S3V_DEV_CONNECTIONS = $Connections -join ";"
$env:S3V_DEV_SCRIPT = $Script
$env:S3V_DEV_OFFSCREEN = "1"
$env:S3V_DEV_DOWNLOAD_DIR = Join-Path $OutDir "downloads"
$env:S3V_DEV_UPLOAD_SOURCE = $UploadSource
$env:S3V_DEV_IMPORT_DIR = $ImportDir
$env:S3V_DEV_PROFILES = $Profiles

# Says so if the app or anything it started (shells, consoles) shows a window
# of its own on the desktop: only the off-screen window of the app may exist.
$strays = @{}
function Watch-StrayWindows([System.Diagnostics.Process]$App) {
    $App.Refresh()
    $family = [DevWindow]::Family([uint32]$App.Id)
    foreach ($window in [DevWindow]::Visible($family, $App.MainWindowHandle)) { $strays[$window] = $true }
}

# Only ever touch the instance started here: the app may also be open for real.
# The debug build is a console program: started through the shell, it
# would get a visible console window of its own, which takes the focus.
$start = New-Object System.Diagnostics.ProcessStartInfo $program
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$app = [System.Diagnostics.Process]::Start($start)
# The moments count from when the window exists, as the app's script does:
# on a busy machine starting can take seconds.
$waited = [Diagnostics.Stopwatch]::StartNew()
do {
    Start-Sleep -Milliseconds 100
    $app.Refresh()
    $rect = New-Object DevWindow+RECT
    $ready = $app.MainWindowHandle -ne 0 -and [DevWindow]::GetWindowRect($app.MainWindowHandle, [ref]$rect) -and ($rect.R - $rect.L) -gt 200
} until ($ready -or $app.HasExited -or $waited.ElapsedMilliseconds -gt 60000)
$clock = [Diagnostics.Stopwatch]::StartNew()
$index = 1
foreach ($moment in $At) {
    while ($clock.ElapsedMilliseconds -lt $moment * 1000) {
        Watch-StrayWindows $app
        Start-Sleep -Milliseconds 150
    }
    Save-WindowCapture $app.Id (Join-Path $OutDir "$Name-$index.png")
    $index++
}
foreach ($window in $strays.Keys) { Write-Warning "window shown on the desktop: $window" }
if (-not $Keep) { Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue }
