<#
.SYNOPSIS
  R0 system sampler (docs/11-background-resource-governance.md section 4).

.DESCRIPTION
  Samples Windows performance counters every -IntervalMs (default 1 s) and writes one CSV row
  per sample. Counters are read through the PDH API with the same English counter paths that
  Get-Counter uses (PdhAddEnglishCounter, so the Chinese UI locale does not matter). One PDH
  query is kept open for the whole run and wildcard instances are re-enumerated on every
  collection, so processes that start or exit mid-run (AI worker, llama-server, WebView2
  renderers) are handled. Measured cost on the target machine is ~15 ms per sample; calling
  Get-Counter once per second instead costs 1-3 s per call.

  In parallel, `nvidia-smi --query-gpu=timestamp,utilization.gpu,memory.used -lms 1000` is
  written to a separate CSV and stopped at the end.

  Process CPU counters are per core (0..100*N). Every proc_* group therefore has both
  proc_<g>_cpu (per-core %) and proc_<g>_cpu_norm (% of the whole machine, divided by the
  logical CPU count).

  Process groups:
    app      photo-view-plus
    apptree  photo-view-plus plus all descendants (WebView2, AI worker, ...)
    wv2      every msedgewebview2 process on the machine (other apps included)
    wv2app   msedgewebview2 processes that descend from photo-view-plus
    python   every python* process on the machine (probe.py included)
    pyapp    python* processes that descend from photo-view-plus (= AI worker)
    llama    llama-server*
    hipsd    HipsDaemon (Huorong real-time protection service)
    hips     every Huorong process (Hips*, usysdiag, wsctrlsvc)
    dwm      dwm (desktop compositor)

  The process tree is rooted at -AppName (default photo-view-plus) via "Creating Process ID".
  Per-process CPU is accounted in 15.6 ms clock ticks, so a single 1 s sample of a small
  process jumps in ~1.56 % (per-core) steps; compare averages, not single samples.

  GPU columns use only the adapter with the largest dedicated memory (the display GPU).
  gpu_<type>_pct is the busiest engine of that type (per-engine sum over processes, like Task
  Manager). An empty cell means "not available", e.g. gpu_compute_pct when the driver exposes
  no Compute/Cuda engine (the case with NVIDIA driver 610.88 on the target machine).

  Besides the CSVs, record_<label>_<run>.meta.json holds run info, counter notes, sampler
  overhead and top_processes (the 15 busiest image names of the run, any process).

.EXAMPLE
  pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\perf\record.ps1 -DurationSec 180 -OutDir D:\perf\r0 -Label S0 -WithProbe
#>
[CmdletBinding()]
param(
    # Run length in seconds; 0 = until Ctrl+C.
    [int]$DurationSec = 180,
    # Output directory (created if missing). Use one directory per measurement session.
    [string]$OutDir = (Join-Path ([System.IO.Path]::GetTempPath()) 'pvp-perf'),
    # Scenario label (S0..S4). Use the same label for repeats; files get a timestamp suffix.
    [string]$Label = 'run',
    [int]$IntervalMs = 1000,
    # Skip the parallel nvidia-smi logger.
    [switch]$NoNvidiaSmi,
    # Also start probe.py (same duration / label / output dir) in this console.
    [switch]$WithProbe,
    # Interpreter for -WithProbe. Use the same one for every scenario.
    [string]$PythonExe = 'python',
    # Print a status line every N samples (0 = quiet).
    [int]$StatusEvery = 10,
    # Image name of the app process that roots the apptree/wv2app/pyapp groups.
    [string]$AppName = 'photo-view-plus'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$csharp = @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.RegularExpressions;

namespace PvpPerf
{
    internal static class Pdh
    {
        public const uint FmtDoubleNoCap = 0x00000200 | 0x00008000; // PDH_FMT_DOUBLE | PDH_FMT_NOCAP100
        public const uint MoreData = 0x800007D2;                     // PDH_MORE_DATA

        [DllImport("pdh.dll", CharSet = CharSet.Unicode)]
        public static extern uint PdhOpenQueryW(string dataSource, IntPtr userData, out IntPtr query);

        [DllImport("pdh.dll", CharSet = CharSet.Unicode)]
        public static extern uint PdhAddEnglishCounterW(IntPtr query, string path, IntPtr userData, out IntPtr counter);

        [DllImport("pdh.dll")]
        public static extern uint PdhCollectQueryData(IntPtr query);

        [DllImport("pdh.dll", CharSet = CharSet.Unicode)]
        public static extern uint PdhGetFormattedCounterArrayW(IntPtr counter, uint format, ref uint bufferSize, out uint itemCount, IntPtr buffer);

        [DllImport("pdh.dll")]
        public static extern uint PdhCloseQuery(IntPtr query);
    }

    internal sealed class ProcRec
    {
        public string Name;   // lower-case image name without .exe
        public int Pid;
        public int Ppid;
        public double Cpu;    // per-core %
        public double Ws;     // Working Set - Private, bytes
    }

    internal sealed class EngineInfo
    {
        public bool Ok;
        public int Pid;
        public string Luid;
        public string Key;    // luid|phys|eng
        public string Type;   // normalised: 3d, compute, copy, videodecode, ...
    }

    public sealed class Sampler : IDisposable
    {
        static readonly string[] ProcGroups = { "app", "apptree", "wv2", "wv2app", "python", "pyapp", "llama", "hipsd", "hips", "dwm" };
        static readonly string[] GpuGroups = { "dwm", "wv2app", "python", "pyapp", "llama" };
        static readonly string[] GpuTypes = { "3d", "compute", "copy", "videodecode" };
        static readonly Regex EngineRx = new Regex(@"^pid_(\d+)_luid_(0x[0-9A-Fa-f]+_0x[0-9A-Fa-f]+)_phys_(\d+)_eng_(\d+)_engtype_(.*)$", RegexOptions.Compiled);
        static readonly Regex ProcMemRx = new Regex(@"^pid_(\d+)_luid_(0x[0-9A-Fa-f]+_0x[0-9A-Fa-f]+)_phys_(\d+)$", RegexOptions.Compiled);
        static readonly Regex AdapterRx = new Regex(@"^luid_(0x[0-9A-Fa-f]+_0x[0-9A-Fa-f]+)_phys_(\d+)$", RegexOptions.Compiled);
        static readonly Regex TypeSuffixRx = new Regex(@"_\d+$", RegexOptions.Compiled);

        IntPtr query = IntPtr.Zero;
        IntPtr buffer = IntPtr.Zero;
        int bufferSize = 0;
        readonly int cpuCount;
        readonly string appName;  // lower-case image name of the app (default photo-view-plus)
        readonly Dictionary<string, IntPtr> counters = new Dictionary<string, IntPtr>();
        readonly List<string> notes = new List<string>();
        bool processV2;
        bool processTree;

        readonly List<string> columns = new List<string>();
        readonly Dictionary<string, int> colIndex = new Dictionary<string, int>();
        double[] row = new double[0];
        string[] diskInstances = new string[0];
        string[] diskIds = new string[0];
        string primaryLuid;

        // scratch buffers, reused every sample
        readonly List<string> names = new List<string>();
        readonly List<double> values = new List<double>();
        readonly List<bool> valid = new List<bool>();
        readonly List<ProcRec> procs = new List<ProcRec>();
        readonly Dictionary<string, double> byName = new Dictionary<string, double>();
        readonly Dictionary<string, double> byName2 = new Dictionary<string, double>();
        readonly Dictionary<int, List<int>> children = new Dictionary<int, List<int>>();
        readonly HashSet<int> appTree = new HashSet<int>();
        readonly Dictionary<string, HashSet<int>> gpuGroupPids = new Dictionary<string, HashSet<int>>();
        readonly HashSet<string> gpuGroupOk = new HashSet<string>(); // groups whose membership is known this sample
        readonly Dictionary<string, EngineInfo> engineCache = new Dictionary<string, EngineInfo>();
        readonly Dictionary<string, double> engineTotals = new Dictionary<string, double>();
        readonly Dictionary<string, string> engineTypes = new Dictionary<string, string>();
        readonly Dictionary<string, Dictionary<string, double>> groupEngine = new Dictionary<string, Dictionary<string, double>>();
        readonly Dictionary<string, double> groupVram = new Dictionary<string, double>();
        // per image name, whole run: catches heavy processes nobody thought of
        readonly Dictionary<string, double> sampleByName = new Dictionary<string, double>();
        readonly Dictionary<string, double> runCpuSum = new Dictionary<string, double>();
        readonly Dictionary<string, double> runCpuMax = new Dictionary<string, double>();
        int runSamples = 0;

        public string LastError;

        public Sampler(int logicalCpus, string app)
        {
            cpuCount = logicalCpus > 0 ? logicalCpus : Environment.ProcessorCount;
            appName = string.IsNullOrEmpty(app) ? "photo-view-plus" : app.ToLowerInvariant();
            if (appName.EndsWith(".exe", StringComparison.Ordinal)) appName = appName.Substring(0, appName.Length - 4);
            uint rc = Pdh.PdhOpenQueryW(null, IntPtr.Zero, out query);
            if (rc != 0) throw new InvalidOperationException("PdhOpenQuery failed: 0x" + rc.ToString("X8"));

            Add("cpu_total", @"\Processor(_Total)\% Processor Time");
            Add("cpu_dpc", @"\Processor(_Total)\% DPC Time");
            Add("cpu_intr", @"\Processor(_Total)\% Interrupt Time");
            Add("cpu_queue", @"\System\Processor Queue Length");
            Add("mem_avail", @"\Memory\Available MBytes");

            // "Process V2" names instances "name:pid" (unique, no #N renumbering when processes exit).
            if (Add("p_cpu", @"\Process V2(*)\% Processor Time"))
            {
                processV2 = true;
                processTree = Add("p_ppid", @"\Process V2(*)\Creating Process ID");
                Add("p_ws", @"\Process V2(*)\Working Set - Private");
            }
            else if (Add("p_cpu", @"\Process(*)\% Processor Time"))
            {
                notes.Add("Process V2 counter set unavailable; using Process (instances aligned by index)");
                bool pid = Add("p_pid", @"\Process(*)\ID Process");
                bool ppid = Add("p_ppid", @"\Process(*)\Creating Process ID");
                processTree = pid && ppid;
                Add("p_ws", @"\Process(*)\Working Set - Private");
            }

            Add("disk_idle", @"\PhysicalDisk(*)\% Idle Time");
            Add("disk_queue", @"\PhysicalDisk(*)\Avg. Disk Queue Length");
            Add("net_rx", @"\Network Interface(*)\Bytes Received/sec");
            Add("gpu_eng", @"\GPU Engine(*)\Utilization Percentage");
            Add("gpu_mem", @"\GPU Adapter Memory(*)\Dedicated Usage");
            Add("gpu_pmem", @"\GPU Process Memory(*)\Dedicated Usage");

            foreach (string g in GpuGroups)
            {
                gpuGroupPids[g] = new HashSet<int>();
                groupEngine[g] = new Dictionary<string, double>();
            }
            if (!processTree) notes.Add("Creating Process ID unavailable: apptree/wv2app/pyapp columns stay empty");
        }

        bool Add(string key, string path)
        {
            IntPtr h;
            uint rc = Pdh.PdhAddEnglishCounterW(query, path, IntPtr.Zero, out h);
            if (rc != 0)
            {
                notes.Add("counter unavailable: " + path + " (0x" + rc.ToString("X8") + ")");
                return false;
            }
            counters[key] = h;
            return true;
        }

        public string[] Notes { get { return notes.ToArray(); } }
        public string[] Columns { get { return columns.ToArray(); } }
        public string[] Disks { get { return diskInstances; } }
        public string PrimaryLuid { get { return primaryLuid; } }
        public bool ProcessV2 { get { return processV2; } }
        public string HeaderCsv { get { return string.Join(",", columns.ToArray()); } }

        // Reads a counter into names/values/valid. Invalid items are kept only when keepInvalid.
        int Read(string key, bool keepInvalid)
        {
            names.Clear(); values.Clear(); valid.Clear();
            IntPtr counter;
            if (!counters.TryGetValue(key, out counter)) return -1;
            for (int attempt = 0; attempt < 4; attempt++)
            {
                uint size = (uint)bufferSize;
                uint count;
                uint rc = Pdh.PdhGetFormattedCounterArrayW(counter, Pdh.FmtDoubleNoCap, ref size, out count, buffer);
                if (rc == Pdh.MoreData)
                {
                    int want = (int)size + 4096;
                    if (buffer != IntPtr.Zero) Marshal.FreeHGlobal(buffer);
                    buffer = Marshal.AllocHGlobal(want);
                    bufferSize = want;
                    continue;
                }
                if (rc != 0) return -1;
                // PDH_FMT_COUNTERVALUE_ITEM_W: { LPWSTR szName; { DWORD CStatus; union { double ... } } }
                // = 24 bytes on x64 and x86: status at +8, value at +16.
                long basePtr = buffer.ToInt64();
                for (int i = 0; i < count; i++)
                {
                    IntPtr item = new IntPtr(basePtr + 24L * i);
                    uint status = (uint)Marshal.ReadInt32(item, 8);
                    bool ok = status == 0 || status == 1; // PDH_CSTATUS_VALID_DATA / NEW_DATA
                    if (!ok && !keepInvalid) continue;
                    names.Add(Marshal.PtrToStringUni(Marshal.ReadIntPtr(item)));
                    values.Add(ok ? BitConverter.Int64BitsToDouble(Marshal.ReadInt64(item, 16)) : double.NaN);
                    valid.Add(ok);
                }
                return names.Count;
            }
            return -1;
        }

        double ReadSingle(string key)
        {
            if (Read(key, false) <= 0) return double.NaN;
            return values[0];
        }

        void AddColumn(string name)
        {
            colIndex[name] = columns.Count;
            columns.Add(name);
        }

        void Set(string col, double v)
        {
            int i;
            if (colIndex.TryGetValue(col, out i)) row[i] = v;
        }

        // "name<TAB>avg per-core %<TAB>max per-core %" for the n busiest image names of the run.
        public string[] TopProcesses(int n)
        {
            List<KeyValuePair<string, double>> list = new List<KeyValuePair<string, double>>(runCpuSum);
            list.Sort(delegate (KeyValuePair<string, double> a, KeyValuePair<string, double> b) { return b.Value.CompareTo(a.Value); });
            int count = Math.Min(n, list.Count);
            string[] result = new string[count];
            for (int i = 0; i < count; i++)
            {
                double avg = runSamples > 0 ? list[i].Value / runSamples : 0;
                result[i] = list[i].Key + "\t" + avg.ToString("0.###", CultureInfo.InvariantCulture) + "\t" +
                    runCpuMax[list[i].Key].ToString("0.###", CultureInfo.InvariantCulture);
            }
            return result;
        }

        public double Last(string col)
        {
            int i;
            if (colIndex.TryGetValue(col, out i)) return row[i];
            return double.NaN;
        }

        static string SafeId(string raw)
        {
            StringBuilder sb = new StringBuilder();
            foreach (char c in raw)
            {
                if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9')) sb.Append(c);
                else if (sb.Length > 0 && sb[sb.Length - 1] != '_') sb.Append('_');
            }
            return sb.ToString().Trim('_');
        }

        // First collection: discovers disk instances and fixes the CSV columns.
        public void Prime()
        {
            // Two collections so rate counters already have a valid baseline.
            Pdh.PdhCollectQueryData(query);
            System.Threading.Thread.Sleep(250);
            Pdh.PdhCollectQueryData(query);
            List<string> disks = new List<string>();
            if (Read("disk_idle", true) > 0)
            {
                foreach (string n in names) if (n != "_Total" && !disks.Contains(n)) disks.Add(n);
            }
            disks.Sort(StringComparer.Ordinal);
            diskInstances = disks.ToArray();
            diskIds = new string[diskInstances.Length];
            for (int i = 0; i < diskInstances.Length; i++) diskIds[i] = SafeId(diskInstances[i]);
            if (diskInstances.Length == 0) notes.Add("no PhysicalDisk instances found");

            AddColumn("cpu_total_pct");
            AddColumn("cpu_dpc_pct");
            AddColumn("cpu_intr_pct");
            AddColumn("cpu_queue_len");
            AddColumn("mem_avail_mb");
            foreach (string g in ProcGroups)
            {
                AddColumn("proc_" + g + "_cpu");
                AddColumn("proc_" + g + "_cpu_norm");
                AddColumn("proc_" + g + "_n");
                AddColumn("proc_" + g + "_ws_mb");
            }
            foreach (string t in GpuTypes) AddColumn("gpu_" + t + "_pct");
            AddColumn("gpu_engine_max_pct");
            foreach (string g in GpuGroups) AddColumn("gpu_" + g + "_pct");
            AddColumn("gpu_dedicated_mb");
            foreach (string g in GpuGroups) AddColumn("vram_" + g + "_mb");
            foreach (string d in diskIds)
            {
                AddColumn("disk_" + d + "_idle_pct");
                AddColumn("disk_" + d + "_busy_pct");
                AddColumn("disk_" + d + "_queue");
            }
            AddColumn("net_rx_MBps");
            AddColumn("net_rx_max_if_MBps");
            AddColumn("sampler_ms");
            row = new double[columns.Count];
            for (int i = 0; i < row.Length; i++) row[i] = double.NaN;
        }

        public string SampleCsv()
        {
            Stopwatch sw = Stopwatch.StartNew();
            for (int i = 0; i < row.Length; i++) row[i] = double.NaN;
            LastError = null;
            uint rc = Pdh.PdhCollectQueryData(query);
            if (rc != 0)
            {
                LastError = "PdhCollectQueryData 0x" + rc.ToString("X8");
            }
            else
            {
                try { Compute(); }
                catch (Exception ex) { LastError = ex.GetType().Name + ": " + ex.Message; }
            }
            Set("sampler_ms", sw.Elapsed.TotalMilliseconds);
            return FormatRow();
        }

        string FormatRow()
        {
            StringBuilder sb = new StringBuilder(row.Length * 6);
            for (int i = 0; i < row.Length; i++)
            {
                if (i > 0) sb.Append(',');
                double v = row[i];
                if (!double.IsNaN(v) && !double.IsInfinity(v)) sb.Append(v.ToString("0.###", CultureInfo.InvariantCulture));
            }
            return sb.ToString();
        }

        static bool IsHips(string n)
        {
            return n.StartsWith("hips", StringComparison.Ordinal) || n == "usysdiag" || n == "wsctrlsvc" || n.StartsWith("huorong", StringComparison.Ordinal);
        }

        void Compute()
        {
            Set("cpu_total_pct", ReadSingle("cpu_total"));
            Set("cpu_dpc_pct", ReadSingle("cpu_dpc"));
            Set("cpu_intr_pct", ReadSingle("cpu_intr"));
            Set("cpu_queue_len", ReadSingle("cpu_queue"));
            Set("mem_avail_mb", ReadSingle("mem_avail"));
            ComputeProcesses();
            ComputeGpu();
            ComputeDisks();
            ComputeNetwork();
        }

        void ComputeProcesses()
        {
            procs.Clear();
            gpuGroupOk.Clear();
            foreach (string g in GpuGroups) gpuGroupPids[g].Clear();
            if (!LoadProcesses()) return; // all proc_* / per-group GPU columns stay empty

            sampleByName.Clear();
            foreach (ProcRec p in procs)
            {
                if (double.IsNaN(p.Cpu)) continue;
                double c;
                sampleByName.TryGetValue(p.Name, out c);
                sampleByName[p.Name] = c + p.Cpu;
            }
            runSamples++;
            foreach (KeyValuePair<string, double> kv in sampleByName)
            {
                double s, m;
                runCpuSum.TryGetValue(kv.Key, out s);
                runCpuSum[kv.Key] = s + kv.Value;
                if (!runCpuMax.TryGetValue(kv.Key, out m) || kv.Value > m) runCpuMax[kv.Key] = kv.Value;
            }

            // descendants of photo-view-plus
            appTree.Clear();
            children.Clear();
            Queue<int> todo = new Queue<int>();
            foreach (ProcRec p in procs)
            {
                if (p.Pid > 0 && p.Ppid > 0 && p.Ppid != p.Pid)
                {
                    List<int> list;
                    if (!children.TryGetValue(p.Ppid, out list)) { list = new List<int>(); children[p.Ppid] = list; }
                    list.Add(p.Pid);
                }
                if (p.Name == appName && p.Pid > 0 && appTree.Add(p.Pid)) todo.Enqueue(p.Pid);
            }
            if (processTree)
            {
                while (todo.Count > 0)
                {
                    int cur = todo.Dequeue();
                    List<int> kids;
                    if (!children.TryGetValue(cur, out kids)) continue;
                    foreach (int k in kids) if (appTree.Add(k)) todo.Enqueue(k);
                }
            }

            foreach (string g in ProcGroups)
            {
                bool treeGroup = g == "apptree" || g == "wv2app" || g == "pyapp";
                if (treeGroup && !processTree) continue; // unavailable -> empty cells
                double cpu = 0, ws = 0;
                int n = 0;
                bool wsOk = true;
                HashSet<int> gpuSet = gpuGroupPids.ContainsKey(g) ? gpuGroupPids[g] : null;
                foreach (ProcRec p in procs)
                {
                    if (!InGroup(g, p)) continue;
                    n++;
                    if (!double.IsNaN(p.Cpu)) cpu += p.Cpu;
                    if (double.IsNaN(p.Ws)) wsOk = false; else ws += p.Ws;
                    if (gpuSet != null && p.Pid > 0) gpuSet.Add(p.Pid);
                }
                // An absent process group uses 0 CPU (not "unknown"), so S0 averages stay meaningful.
                Set("proc_" + g + "_n", n);
                Set("proc_" + g + "_cpu", cpu);
                Set("proc_" + g + "_cpu_norm", cpu / cpuCount);
                if (n > 0 && wsOk) Set("proc_" + g + "_ws_mb", ws / 1048576.0);
                if (gpuSet != null) gpuGroupOk.Add(g);
            }
        }

        bool LoadProcesses()
        {
            if (processV2)
            {
                byName.Clear(); byName2.Clear();
                if (processTree && Read("p_ppid", false) > 0)
                    for (int i = 0; i < names.Count; i++) byName[names[i]] = values[i];
                if (Read("p_ws", false) > 0)
                    for (int i = 0; i < names.Count; i++) byName2[names[i]] = values[i];
                if (Read("p_cpu", true) <= 0) return false;
                for (int i = 0; i < names.Count; i++)
                {
                    string inst = names[i];
                    int colon = inst.LastIndexOf(':');
                    if (colon <= 0) continue;
                    int pid;
                    if (!int.TryParse(inst.Substring(colon + 1), NumberStyles.Integer, CultureInfo.InvariantCulture, out pid)) continue;
                    if (pid == 0) continue; // Idle
                    ProcRec p = new ProcRec();
                    p.Name = inst.Substring(0, colon).ToLowerInvariant();
                    p.Pid = pid;
                    p.Cpu = values[i];
                    double d;
                    p.Ppid = byName.TryGetValue(inst, out d) && !double.IsNaN(d) ? (int)d : -1;
                    p.Ws = byName2.TryGetValue(inst, out d) ? d : double.NaN;
                    procs.Add(p);
                }
            }
            else
            {
                if (Read("p_cpu", true) <= 0) return false;
                List<string> n0 = new List<string>(names);
                List<double> cpu = new List<double>(values);
                List<double> pids = new List<double>();
                List<double> ppids = new List<double>();
                List<double> ws = new List<double>();
                if (counters.ContainsKey("p_pid") && Read("p_pid", true) == n0.Count) pids.AddRange(values);
                if (counters.ContainsKey("p_ppid") && Read("p_ppid", true) == n0.Count) ppids.AddRange(values);
                if (counters.ContainsKey("p_ws") && Read("p_ws", true) == n0.Count) ws.AddRange(values);
                for (int i = 0; i < n0.Count; i++)
                {
                    string nm = n0[i];
                    if (nm == "_Total" || nm == "Idle") continue;
                    int hash = nm.IndexOf('#');
                    ProcRec p = new ProcRec();
                    p.Name = (hash > 0 ? nm.Substring(0, hash) : nm).ToLowerInvariant();
                    p.Pid = pids.Count == n0.Count && !double.IsNaN(pids[i]) ? (int)pids[i] : -1;
                    p.Ppid = ppids.Count == n0.Count && !double.IsNaN(ppids[i]) ? (int)ppids[i] : -1;
                    p.Cpu = cpu[i];
                    p.Ws = ws.Count == n0.Count ? ws[i] : double.NaN;
                    procs.Add(p);
                }
            }

            return true;
        }

        bool InGroup(string g, ProcRec p)
        {
            string n = p.Name;
            switch (g)
            {
                case "app": return n == appName;
                case "apptree": return appTree.Contains(p.Pid);
                case "wv2": return n.StartsWith("msedgewebview2", StringComparison.Ordinal);
                case "wv2app": return n.StartsWith("msedgewebview2", StringComparison.Ordinal) && appTree.Contains(p.Pid);
                case "python": return n.StartsWith("python", StringComparison.Ordinal);
                case "pyapp": return n.StartsWith("python", StringComparison.Ordinal) && appTree.Contains(p.Pid);
                case "llama": return n.StartsWith("llama-server", StringComparison.Ordinal);
                case "hipsd": return n == "hipsdaemon";
                case "hips": return IsHips(n);
                case "dwm": return n == "dwm";
            }
            return false;
        }

        EngineInfo ParseEngine(string inst)
        {
            EngineInfo e;
            if (engineCache.TryGetValue(inst, out e)) return e;
            e = new EngineInfo();
            Match m = EngineRx.Match(inst);
            if (m.Success)
            {
                e.Ok = true;
                e.Pid = int.Parse(m.Groups[1].Value, CultureInfo.InvariantCulture);
                e.Luid = m.Groups[2].Value.ToLowerInvariant();
                e.Key = e.Luid + "|" + m.Groups[3].Value + "|" + m.Groups[4].Value;
                string t = TypeSuffixRx.Replace(m.Groups[5].Value.ToLowerInvariant(), "");
                if (t == "cuda") t = "compute";
                e.Type = t;
            }
            if (engineCache.Count > 50000) engineCache.Clear();
            engineCache[inst] = e;
            return e;
        }

        void ComputeGpu()
        {
            // primary adapter = largest dedicated usage
            primaryLuid = null;
            double best = -1;
            if (Read("gpu_mem", false) > 0)
            {
                for (int i = 0; i < names.Count; i++)
                {
                    Match m = AdapterRx.Match(names[i]);
                    if (!m.Success) continue;
                    if (values[i] > best) { best = values[i]; primaryLuid = m.Groups[1].Value.ToLowerInvariant(); }
                }
                if (best >= 0) Set("gpu_dedicated_mb", best / 1048576.0);
            }

            engineTotals.Clear();
            foreach (string g in GpuGroups) groupEngine[g].Clear();
            if (Read("gpu_eng", false) >= 0)
            {
                for (int i = 0; i < names.Count; i++)
                {
                    EngineInfo e = ParseEngine(names[i]);
                    if (!e.Ok) continue;
                    if (primaryLuid != null && e.Luid != primaryLuid) continue;
                    double v = values[i];
                    if (double.IsNaN(v)) continue;
                    double cur;
                    engineTotals.TryGetValue(e.Key, out cur);
                    engineTotals[e.Key] = cur + v;
                    engineTypes[e.Key] = e.Type;
                    foreach (string g in GpuGroups)
                    {
                        if (!gpuGroupPids[g].Contains(e.Pid)) continue;
                        Dictionary<string, double> ge = groupEngine[g];
                        double gc;
                        ge.TryGetValue(e.Key, out gc);
                        ge[e.Key] = gc + v;
                    }
                }
                double maxAll = 0;
                Dictionary<string, double> typeMax = new Dictionary<string, double>();
                foreach (KeyValuePair<string, double> kv in engineTotals)
                {
                    double v = Math.Min(100.0, kv.Value);
                    if (v > maxAll) maxAll = v;
                    string t = engineTypes[kv.Key];
                    double tm;
                    if (!typeMax.TryGetValue(t, out tm) || v > tm) typeMax[t] = v;
                }
                foreach (string t in GpuTypes)
                {
                    double tm;
                    if (typeMax.TryGetValue(t, out tm)) Set("gpu_" + t + "_pct", tm);
                }
                if (engineTotals.Count > 0) Set("gpu_engine_max_pct", maxAll);
                foreach (string g in GpuGroups)
                {
                    if (!gpuGroupOk.Contains(g)) continue;
                    double gm = 0;
                    foreach (KeyValuePair<string, double> kv in groupEngine[g]) if (kv.Value > gm) gm = kv.Value;
                    Set("gpu_" + g + "_pct", Math.Min(100.0, gm));
                }
            }

            groupVram.Clear();
            if (Read("gpu_pmem", false) >= 0)
            {
                for (int i = 0; i < names.Count; i++)
                {
                    Match m = ProcMemRx.Match(names[i]);
                    if (!m.Success) continue;
                    if (primaryLuid != null && m.Groups[2].Value.ToLowerInvariant() != primaryLuid) continue;
                    int pid = int.Parse(m.Groups[1].Value, CultureInfo.InvariantCulture);
                    foreach (string g in GpuGroups)
                    {
                        if (!gpuGroupPids[g].Contains(pid)) continue;
                        double cur;
                        groupVram.TryGetValue(g, out cur);
                        groupVram[g] = cur + values[i];
                    }
                }
                foreach (string g in GpuGroups)
                {
                    if (!gpuGroupOk.Contains(g)) continue;
                    double v;
                    groupVram.TryGetValue(g, out v);
                    Set("vram_" + g + "_mb", v / 1048576.0);
                }
            }
        }

        void ComputeDisks()
        {
            if (diskInstances.Length == 0) return;
            byName.Clear();
            if (Read("disk_idle", false) > 0)
                for (int i = 0; i < names.Count; i++) byName[names[i]] = values[i];
            byName2.Clear();
            if (Read("disk_queue", false) > 0)
                for (int i = 0; i < names.Count; i++) byName2[names[i]] = values[i];
            for (int d = 0; d < diskInstances.Length; d++)
            {
                double v;
                if (byName.TryGetValue(diskInstances[d], out v) && !double.IsNaN(v))
                {
                    double idle = Math.Max(0.0, Math.Min(100.0, v));
                    Set("disk_" + diskIds[d] + "_idle_pct", idle);
                    Set("disk_" + diskIds[d] + "_busy_pct", 100.0 - idle);
                }
                if (byName2.TryGetValue(diskInstances[d], out v)) Set("disk_" + diskIds[d] + "_queue", v);
            }
        }

        void ComputeNetwork()
        {
            if (Read("net_rx", false) < 0) return;
            double sum = 0, max = 0;
            for (int i = 0; i < values.Count; i++)
            {
                double v = values[i];
                if (double.IsNaN(v)) continue;
                sum += v;
                if (v > max) max = v;
            }
            Set("net_rx_MBps", sum / 1e6);
            Set("net_rx_max_if_MBps", max / 1e6);
        }

        public void Dispose()
        {
            if (query != IntPtr.Zero) { Pdh.PdhCloseQuery(query); query = IntPtr.Zero; }
            if (buffer != IntPtr.Zero) { Marshal.FreeHGlobal(buffer); buffer = IntPtr.Zero; bufferSize = 0; }
        }
    }
}
'@

function Get-SafeLabel([string]$s) {
    $t = ($s -replace '[\\/:*?"<>|\s,]+', '_').Trim('_')
    if ([string]::IsNullOrEmpty($t)) { $t = 'run' }
    return $t
}

function ConvertTo-ArgLine([string[]]$items) {
    $parts = foreach ($a in $items) {
        if ($a -match '[\s"]') { '"' + ($a -replace '"', '\"') + '"' } else { $a }
    }
    return ($parts -join ' ')
}

function Format-Num($v, [string]$fmt = '0.0') {
    if ($null -eq $v -or [double]::IsNaN([double]$v)) { return '-' }
    return ([double]$v).ToString($fmt, [System.Globalization.CultureInfo]::InvariantCulture)
}

if ($IntervalMs -lt 200) { throw 'IntervalMs must be >= 200' }

$Label = Get-SafeLabel $Label
$OutDir = [System.IO.Path]::GetFullPath($OutDir).TrimEnd('\')
if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$runId = Get-Date -Format 'yyyyMMdd-HHmmss'
$csvPath = Join-Path $OutDir ("record_{0}_{1}.csv" -f $Label, $runId)
$nvPath = Join-Path $OutDir ("nvidia_{0}_{1}.csv" -f $Label, $runId)
$nvErrPath = Join-Path $OutDir ("nvidia_{0}_{1}.err.txt" -f $Label, $runId)
$metaPath = Join-Path $OutDir ("record_{0}_{1}.meta.json" -f $Label, $runId)
$inv = [System.Globalization.CultureInfo]::InvariantCulture
$cpuCount = [Environment]::ProcessorCount

if (-not ('PvpPerf.Sampler' -as [type])) {
    Add-Type -TypeDefinition $csharp -Language CSharp
}

$sampler = $null
$writer = $null
$nv = $null
$probe = $null
$samples = 0
$errors = 0
$samplerMsSum = 0.0
$samplerMsMax = 0.0
$selfCpuSum = 0.0
$startTime = Get-Date
$nvNote = $null
$probeNote = $null

try {
    $sampler = New-Object PvpPerf.Sampler($cpuCount, $AppName)
    $sampler.Prime()

    $writer = New-Object System.IO.StreamWriter($csvPath, $false, (New-Object System.Text.UTF8Encoding($true)))
    $writer.AutoFlush = $true
    $writer.WriteLine('timestamp,elapsed_s,label,' + $sampler.HeaderCsv + ',self_cpu_pct')

    if (-not $NoNvidiaSmi) {
        $nvExe = Get-Command nvidia-smi.exe -ErrorAction SilentlyContinue
        if ($null -eq $nvExe) {
            $nvNote = 'nvidia-smi not found on PATH; skipped'
        } else {
            # Header is printed once; stdout redirection is line-flushed (the -f option is not).
            $nv = Start-Process -FilePath $nvExe.Path -NoNewWindow -PassThru `
                -ArgumentList '--query-gpu=timestamp,utilization.gpu,memory.used --format=csv,nounits -lms 1000' `
                -RedirectStandardOutput $nvPath -RedirectStandardError $nvErrPath
        }
    }

    if ($WithProbe) {
        $probeScript = Join-Path $PSScriptRoot 'probe.py'
        $probeArgs = ConvertTo-ArgLine @($probeScript, '--duration', "$DurationSec", '--out', $OutDir, '--label', $Label, '--run-id', $runId)
        try {
            $probe = Start-Process -FilePath $PythonExe -ArgumentList $probeArgs -NoNewWindow -PassThru
        } catch {
            $probeNote = "failed to start probe.py: $($_.Exception.Message)"
            Write-Warning $probeNote
        }
    }

    Write-Host ("[record] label={0} duration={1}s interval={2}ms cpus={3} process-set={4}" -f $Label, $DurationSec, $IntervalMs, $cpuCount, $(if ($sampler.ProcessV2) { 'Process V2' } else { 'Process' }))
    Write-Host ("[record] csv     : {0}" -f $csvPath)
    if ($nv) { Write-Host ("[record] nvidia  : {0}" -f $nvPath) } elseif ($nvNote) { Write-Host "[record] $nvNote" }
    foreach ($n in $sampler.Notes) { Write-Host "[record] note: $n" }

    $me = [System.Diagnostics.Process]::GetCurrentProcess()
    $lastSelfCpu = $me.TotalProcessorTime.TotalMilliseconds
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $lastWall = 0.0
    $maxSamples = if ($DurationSec -gt 0) { [int][Math]::Floor($DurationSec * 1000.0 / $IntervalMs) } else { [int]::MaxValue }
    $target = [int64]$IntervalMs

    while ($samples -lt $maxSamples) {
        $wait = $target - $sw.ElapsedMilliseconds
        if ($wait -gt 0) { Start-Sleep -Milliseconds ([int]$wait) }

        $ts = [DateTimeOffset]::Now.ToString("yyyy-MM-dd'T'HH:mm:ss.fffzzz", $inv)
        $wall = $sw.Elapsed.TotalMilliseconds
        $values = $sampler.SampleCsv()
        if ($sampler.LastError) {
            $errors++
            if ($errors -le 5) { Write-Warning ("[record] sample error: {0}" -f $sampler.LastError) }
        }
        $me.Refresh()
        $selfCpu = $me.TotalProcessorTime.TotalMilliseconds
        $selfPct = if ($wall -gt $lastWall) { ($selfCpu - $lastSelfCpu) * 100.0 / ($wall - $lastWall) } else { 0.0 }
        $lastSelfCpu = $selfCpu
        $lastWall = $wall
        $writer.WriteLine(('{0},{1},{2},{3},{4}' -f $ts, ($wall / 1000.0).ToString('0.000', $inv), $Label, $values, $selfPct.ToString('0.##', $inv)))
        $samples++

        $sms = $sampler.Last('sampler_ms')
        $samplerMsSum += $sms
        if ($sms -gt $samplerMsMax) { $samplerMsMax = $sms }
        $selfCpuSum += $selfPct

        if ($StatusEvery -gt 0 -and ($samples % $StatusEvery) -eq 0) {
            Write-Host ("[record {0} {1,4}s] cpu {2}% | app-tree {3}% | pyapp {4}% | hips {5}% | dwm {6}% | gpu3d {7}% | vram {8}MB | net {9}MB/s | avail {10}MB" -f `
                $Label, [int]($wall / 1000), (Format-Num $sampler.Last('cpu_total_pct')), (Format-Num $sampler.Last('proc_apptree_cpu_norm')),
                (Format-Num $sampler.Last('proc_pyapp_cpu_norm')), (Format-Num $sampler.Last('proc_hips_cpu_norm')), (Format-Num $sampler.Last('proc_dwm_cpu_norm')),
                (Format-Num $sampler.Last('gpu_3d_pct')), (Format-Num $sampler.Last('gpu_dedicated_mb') '0'), (Format-Num $sampler.Last('net_rx_MBps') '0.00'),
                (Format-Num $sampler.Last('mem_avail_mb') '0'))
        }

        $target += $IntervalMs
        # After a stall (sleep/suspend, debugger) resync instead of firing a burst of samples.
        if ($sw.ElapsedMilliseconds -gt $target + $IntervalMs) { $target = $sw.ElapsedMilliseconds + $IntervalMs }
    }
}
finally {
    if ($nv -and -not $nv.HasExited) { try { Stop-Process -Id $nv.Id -Force -ErrorAction SilentlyContinue } catch { } }
    if ($writer) { $writer.Flush(); $writer.Dispose() }
    if ((Test-Path -LiteralPath $nvErrPath) -and ((Get-Item -LiteralPath $nvErrPath).Length -eq 0)) {
        Remove-Item -LiteralPath $nvErrPath -ErrorAction SilentlyContinue
    }
    if ($probe) {
        # probe.py has the same duration (+ ~2 s calibration); give it time to write its last window.
        if (-not $probe.WaitForExit(20000)) {
            try { Stop-Process -Id $probe.Id -Force -ErrorAction SilentlyContinue } catch { }
            $probeNote = 'probe.py did not exit in time and was killed'
        }
    }
    $endTime = Get-Date
    if ($sampler) {
        $top = @(foreach ($entry in $sampler.TopProcesses(15)) {
            $parts = $entry -split "`t"
            $avg = [double]::Parse($parts[1], $inv)
            [ordered]@{
                name         = $parts[0]
                cpu_avg      = $avg
                cpu_avg_norm = [Math]::Round($avg / $cpuCount, 3)
                cpu_max      = [double]::Parse($parts[2], $inv)
            }
        })
        $meta = [ordered]@{
            label             = $Label
            run_id            = $runId
            start             = $startTime.ToString('o')
            end               = $endTime.ToString('o')
            duration_sec      = $DurationSec
            interval_ms       = $IntervalMs
            samples           = $samples
            sample_errors     = $errors
            logical_cpus      = $cpuCount
            app_name          = $AppName
            computer          = $env:COMPUTERNAME
            os                = [Environment]::OSVersion.VersionString
            powershell        = $PSVersionTable.PSVersion.ToString()
            process_counters  = $(if ($sampler.ProcessV2) { 'Process V2' } else { 'Process' })
            primary_gpu_luid  = $sampler.PrimaryLuid
            disks             = @($sampler.Disks)
            notes             = @($sampler.Notes)
            sampler_ms_avg    = $(if ($samples) { [Math]::Round($samplerMsSum / $samples, 2) } else { $null })
            sampler_ms_max    = [Math]::Round($samplerMsMax, 2)
            self_cpu_pct_avg  = $(if ($samples) { [Math]::Round($selfCpuSum / $samples, 2) } else { $null })
            record_csv        = (Split-Path -Leaf $csvPath)
            nvidia_csv        = $(if ($nv) { Split-Path -Leaf $nvPath } else { $null })
            nvidia_note       = $nvNote
            probe_note        = $probeNote
            # busiest image names over the whole run (per-core %, and /logical CPUs)
            top_processes     = $top
        }
        $meta | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $metaPath -Encoding UTF8
        $sampler.Dispose()
        Write-Host ("[record] done: {0} samples, sampler avg {1} ms / max {2} ms, self CPU avg {3}% of one core" -f $samples, $meta.sampler_ms_avg, $meta.sampler_ms_max, $meta.self_cpu_pct_avg)
        if ($probeNote) { Write-Warning "[record] $probeNote" }
    }
}
