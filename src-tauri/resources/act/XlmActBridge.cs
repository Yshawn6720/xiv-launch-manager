// XLM ACT Bridge —— 由 xiv-launch-manager（XLM）寫在 %LOCALAPPDATA%\xiv-launch-manager\act\、登記到 ACT 設定檔的 ACT 外掛
//
// 用途
//   XLM 啟動某帳號的 FFXIV 成功後，會把該遊戲程序的 pid 與啟動時間寫進指令檔。
//   本外掛讀取指令檔，把「本 ACT」中 FFXIV_ACT_Plugin 設定頁的「FFXIV Process ID」
//   切到那個遊戲程序：先按「Refresh List」更新清單，再選取該 pid。
//   雙開 ACT 時一個固定國際服、一個固定繁中服，所以本外掛只「讀」FFXIV 插件
//   目前選取的 Region（cboRegion：Global → global、繁體中文 → tw），只取指令檔中
//   對應的那一項；其他 Region 一律不連動。本外掛絕不切換 Region。
//
// 只接受的輸入
//   - 固定路徑 %APPDATA%\xiv-launch-manager\state\act_target.json，不接受其他路徑。
//   - 檔案不超過 4096 bytes、UTF-8、無 BOM。
//   - 內容恰為 JSON 物件：
//       {"tw":{"pid":<u32>,"created":<u64>},"global":{"pid":<u32>,"created":<u64>}}
//     頂層只允許 tw、global（皆可省略）；每項必須恰有 pid、created 兩個非負整數欄位。
//     多欄、缺欄、型別不對、重複 key、數值超出範圍 → 整檔不採用，只更新狀態文字。
//   - 套用前確認該 pid 的程序名稱是 ffxiv_dx11，且 Process.StartTime.ToFileTimeUtc()
//     等於 created（Windows FILETIME，UTC，100ns）；不符（pid 被重用、指令檔過時）就不動作。
//   - 同一組 (區服, pid, created) 套用成功後不重複套用。
//
// 不做的事
//   - 不碰遊戲程序：只查詢程序名稱與啟動時間，不讀寫其記憶體、不注入、不結束。
//   - 除上述指令檔外不讀寫任何路徑、不寫任何檔案、不執行或載入任何東西；
//     指令檔內容不會被當成路徑或命令使用。
//   - 不切換 FFXIV 插件的 Region，也不改它 Process ID 以外的設定。
//   - 不使用跨程序共用的可寫資源（具名 mutex、共用檔案等）：同一份 .cs 會被兩個 ACT
//     各自載入，兩邊只各自讀指令檔。
//
// 為何依控制項名稱操作
//   FFXIV_ACT_Plugin 沒有公開的 API、命令列、環境變數或設定檔可指定 Process ID，只能
//   操作它設定頁上的控制項：ComboBox「cboRegion」、ComboBox「cboProcessID」、
//   Button「Refresh List」（Name 為 cmdReloadProcess；以 Name 或 Text 尋找）。
//   插件改版改名時，狀態文字會寫明找不到哪個控制項。
//
// 編譯環境
//   ACT 於執行期以 .NET Framework 的 CodeDom C# 編譯器編譯本檔（C# 5），預設參考
//   mscorlib、System、System.Data、System.Linq、System.Drawing、System.Xml、
//   System.Windows.Forms 與 ACT 本身（未含 System.Core，故不用 LINQ）。本檔只用
//   System、System.Windows.Forms 與 ACT 的型別。

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Reflection;
using System.Text;
using System.Windows.Forms;
using Advanced_Combat_Tracker;

[assembly: AssemblyTitle("XLM ACT Bridge")]
[assembly: AssemblyDescription("Follows xiv-launch-manager game launches: selects the launched game in FFXIV_ACT_Plugin's FFXIV Process ID.")]
[assembly: AssemblyCompany("xiv-launch-manager")]
[assembly: AssemblyVersion("1.0.0.0")]

namespace XlmActBridge
{
    public sealed class XlmActBridgePlugin : IActPluginV1
    {
        private const int DebounceMs = 300;
        private const int BusyRetryMs = 200;
        private const int MaxBusyRetries = 10;
        private const int ControlRetryMs = 2000;
        private const int RetryWindowMs = 60000;
        private const int WatcherCheckMs = 5000;
        private const int MaxLogLines = 30;
        private const int MaxLabelChars = 120;

        // 外掛分頁最上方的固定說明（以跳脫碼避免原始碼編碼問題）：
        //   xiv-launch-manager 的 ACT 連動：登入遊戲後自動把 FFXIV 插件的 Process ID 切到該遊戲（依本 ACT 的 Region）。
        //   不會改 Region 或其他設定。
        private const string AboutText =
            "xiv-launch-manager \u7684 ACT \u9023\u52D5\uFF1A\u767B\u5165\u904A\u6232\u5F8C\u81EA\u52D5\u628A FFXIV \u63D2\u4EF6\u7684 Process ID \u5207\u5230\u8A72\u904A\u6232\uFF08\u4F9D\u672C ACT \u7684 Region\uFF09\u3002\r\n"
            + "\u4E0D\u6703\u6539 Region \u6216\u5176\u4ED6\u8A2D\u5B9A\u3002";

        private readonly List<string> _log = new List<string>();
        private Label _statusLabel;
        private TextBox _statusBox;
        private Control _uiControl;
        private System.Windows.Forms.Timer _workTimer;
        private System.Windows.Forms.Timer _watcherTimer;
        private FileSystemWatcher _watcher;
        private ComboBox _hookedRegionCombo;
        private volatile bool _active;
        private volatile string _postFailure;
        private string _stateDir;
        private string _targetPath;
        private string _regionDisplay = "尚未判定";
        private string _lastAppliedKey;
        private string _lastMessage;
        private DateTime _retryDeadlineUtc;
        private int _busyRetries;

        public void InitPlugin(TabPage pluginScreenSpace, Label pluginStatusText)
        {
            Start(pluginScreenSpace, pluginStatusText, Bridge.StateDirectory(), ActGlobals.oFormActMain);
        }

        // uiControl：用來把背景執行緒的檔案事件轉回 UI 執行緒（ACT 主視窗）。
        internal void Start(TabPage pluginScreenSpace, Label pluginStatusText, string stateDir, Control uiControl)
        {
            _statusLabel = pluginStatusText;
            try
            {
                _stateDir = stateDir;
                _targetPath = Path.Combine(_stateDir, Bridge.TargetFileName);
                _uiControl = uiControl;

                _statusBox = new TextBox();
                _statusBox.Multiline = true;
                _statusBox.ReadOnly = true;
                _statusBox.WordWrap = true;
                _statusBox.ScrollBars = ScrollBars.Vertical;
                _statusBox.Dock = DockStyle.Fill;
                pluginScreenSpace.Controls.Add(_statusBox);
                Label about = new Label();
                about.Text = AboutText;
                about.AutoSize = true;
                about.Padding = new Padding(4);
                about.Dock = DockStyle.Top;
                pluginScreenSpace.Controls.Add(about);
                // Dock 依 z-order 由後往前排版：說明（Top）先佔上方，狀態框（Fill）再填滿剩下的空間。
                _statusBox.BringToFront();
                pluginScreenSpace.Text = "XLM ACT Bridge";

                _workTimer = new System.Windows.Forms.Timer();
                _workTimer.Tick += OnWorkTimerTick;
                _watcherTimer = new System.Windows.Forms.Timer();
                _watcherTimer.Interval = WatcherCheckMs;
                _watcherTimer.Tick += OnWatcherTimerTick;

                _active = true;
                SetStatus("已啟動，監看 " + _targetPath);
                EnsureWatcher();
                _watcherTimer.Start();
                // ACT 比遊戲晚開時，指令檔可能已存在：啟動後也檢查一次。
                Trigger();
            }
            catch (Exception ex)
            {
                SetStatus("初始化失敗：" + ex.Message);
            }
        }

        // ACT 以 try/catch 包住 DeInitPlugin；這裡的釋放步驟各自獨立，前一步失敗也要繼續釋放後面的資源。
        public void DeInitPlugin()
        {
            _active = false;
            string failure = null;
            try
            {
                DisposeTimers();
            }
            catch (Exception ex)
            {
                failure = "停止 timer：" + ex.Message;
            }
            try
            {
                DisposeWatcher();
            }
            catch (Exception ex)
            {
                failure = "釋放指令檔監看：" + ex.Message;
            }
            try
            {
                UnhookRegionCombo();
            }
            catch (Exception ex)
            {
                failure = "解除 Region 事件：" + ex.Message;
            }
            if (_statusLabel != null && !_statusLabel.IsDisposed)
            {
                _statusLabel.Text = failure == null
                    ? "XLM ACT Bridge 已停止"
                    : "XLM ACT Bridge 已停止（" + failure + "）";
            }
        }

        private void DisposeTimers()
        {
            System.Windows.Forms.Timer work = _workTimer;
            _workTimer = null;
            if (work != null)
            {
                work.Stop();
                work.Dispose();
            }
            System.Windows.Forms.Timer watcherTimer = _watcherTimer;
            _watcherTimer = null;
            if (watcherTimer != null)
            {
                watcherTimer.Stop();
                watcherTimer.Dispose();
            }
        }

        // ---- 觸發與排程（全部在 ACT UI 執行緒） ----

        // 新事件（啟動、指令檔變動、監看重建、Region 被改）：重設 60 秒重試期限，去彈跳後執行。
        private void Trigger()
        {
            if (!_active || _workTimer == null)
            {
                return;
            }
            _retryDeadlineUtc = DateTime.UtcNow.AddMilliseconds(RetryWindowMs);
            _busyRetries = 0;
            ScheduleWork(DebounceMs);
        }

        private void ScheduleWork(int delayMs)
        {
            _workTimer.Stop();
            _workTimer.Interval = delayMs;
            _workTimer.Start();
        }

        private void OnWorkTimerTick(object sender, EventArgs e)
        {
            try
            {
                if (_workTimer != null)
                {
                    _workTimer.Stop();
                }
                if (_active)
                {
                    RunOnce();
                }
            }
            catch (Exception ex)
            {
                SetStatus("內部錯誤：" + ex.Message);
            }
        }

        private void RunOnce()
        {
            string text;
            string error;
            ReadStatus readStatus = Bridge.ReadTargetText(_targetPath, out text, out error);
            if (readStatus == ReadStatus.NotFound)
            {
                SetStatus("尚無指令檔，等待 XLM 啟動遊戲");
                return;
            }
            if (readStatus == ReadStatus.Busy)
            {
                if (_busyRetries < MaxBusyRetries)
                {
                    _busyRetries++;
                    ScheduleWork(BusyRetryMs);
                    return;
                }
                SetStatus("指令檔持續無法讀取，放棄這次：" + error);
                return;
            }
            if (readStatus != ReadStatus.Ok)
            {
                SetStatus("指令檔不採用：" + error);
                return;
            }
            _busyRetries = 0;

            TargetFile target = Bridge.ParseTarget(text, out error);
            if (target == null)
            {
                SetStatus("指令檔格式不符，整檔不採用：" + error);
                return;
            }

            string why;
            FfxivControls controls = FindFfxivControlsInAct(out why);
            if (controls == null)
            {
                RetryOrGiveUp(why);
                return;
            }
            HookRegionCombo(controls.Region);

            Outcome outcome = Bridge.ApplyTarget(target, controls, _lastAppliedKey, Bridge.GameProcessName);
            _regionDisplay = outcome.RegionDisplay;
            if (outcome.Kind == OutcomeKind.Retry)
            {
                RetryOrGiveUp(outcome.Message);
                return;
            }
            if (outcome.Kind == OutcomeKind.Applied)
            {
                _lastAppliedKey = outcome.AppliedKey;
            }
            SetStatus(outcome.Message);
        }

        private void RetryOrGiveUp(string why)
        {
            if (DateTime.UtcNow < _retryDeadlineUtc)
            {
                SetStatus("2 秒後重試：" + why);
                ScheduleWork(ControlRetryMs);
            }
            else
            {
                SetStatus("已重試 60 秒仍失敗，放棄：" + why);
            }
        }

        private static FfxivControls FindFfxivControlsInAct(out string why)
        {
            FormActMain main = ActGlobals.oFormActMain;
            if (main == null || main.ActPlugins == null)
            {
                why = "ACT 主視窗尚未就緒";
                return null;
            }
            ActPluginData found = null;
            bool listedButNotReady = false;
            foreach (ActPluginData plugin in main.ActPlugins)
            {
                if (plugin == null || plugin.pluginFile == null
                    || !string.Equals(plugin.pluginFile.Name, Bridge.FfxivPluginFileName, StringComparison.OrdinalIgnoreCase))
                {
                    continue;
                }
                if (plugin.cbEnabled != null && plugin.cbEnabled.Checked && plugin.pluginObj != null
                    && plugin.tpPluginSpace != null && !plugin.tpPluginSpace.IsDisposed)
                {
                    found = plugin;
                    break;
                }
                listedButNotReady = true;
            }
            if (found == null)
            {
                why = listedButNotReady
                    ? Bridge.FfxivPluginFileName + " 未啟用或尚未載入完成"
                    : "ACT 外掛清單中找不到已啟用的 " + Bridge.FfxivPluginFileName;
                return null;
            }
            string missing;
            FfxivControls controls = Bridge.FindFfxivControls(found.tpPluginSpace, out missing);
            why = controls == null ? "FFXIV 插件畫面中找不到控制項：" + missing : null;
            return controls;
        }

        // ---- Region 被手動改變時重新判斷 ----

        private void HookRegionCombo(ComboBox combo)
        {
            if (ReferenceEquals(combo, _hookedRegionCombo))
            {
                return;
            }
            UnhookRegionCombo();
            combo.SelectedIndexChanged += OnRegionChanged;
            _hookedRegionCombo = combo;
            // FFXIV 插件重新載入後是新的畫面（Process ID 回到預設），允許再套用一次。
            _lastAppliedKey = null;
        }

        private void UnhookRegionCombo()
        {
            if (_hookedRegionCombo != null)
            {
                _hookedRegionCombo.SelectedIndexChanged -= OnRegionChanged;
                _hookedRegionCombo = null;
            }
        }

        private void OnRegionChanged(object sender, EventArgs e)
        {
            try
            {
                Trigger();
            }
            catch (Exception ex)
            {
                SetStatus("內部錯誤（Region 變更）：" + ex.Message);
            }
        }

        // ---- 監看指令檔 ----

        private void EnsureWatcher()
        {
            if (_watcher != null)
            {
                if (Directory.Exists(_stateDir))
                {
                    return;
                }
                // 資料夾被刪除：舊監看已失效，等資料夾重新出現再建立。
                DisposeWatcher();
                return;
            }
            if (!Directory.Exists(_stateDir))
            {
                return;
            }
            FileSystemWatcher watcher = new FileSystemWatcher(_stateDir, Bridge.TargetFileName);
            try
            {
                watcher.IncludeSubdirectories = false;
                watcher.NotifyFilter = NotifyFilters.FileName | NotifyFilters.LastWrite | NotifyFilters.Size | NotifyFilters.CreationTime;
                watcher.Created += OnWatcherEvent;
                watcher.Changed += OnWatcherEvent;
                watcher.Renamed += OnWatcherEvent;
                watcher.Error += OnWatcherError;
                watcher.EnableRaisingEvents = true;
            }
            catch
            {
                watcher.Dispose();
                throw;
            }
            _watcher = watcher;
            // 監看建立前可能已寫入指令檔。
            Trigger();
        }

        private void DisposeWatcher()
        {
            FileSystemWatcher watcher = _watcher;
            _watcher = null;
            if (watcher != null)
            {
                watcher.EnableRaisingEvents = false;
                watcher.Created -= OnWatcherEvent;
                watcher.Changed -= OnWatcherEvent;
                watcher.Renamed -= OnWatcherEvent;
                watcher.Error -= OnWatcherError;
                watcher.Dispose();
            }
        }

        private void OnWatcherTimerTick(object sender, EventArgs e)
        {
            try
            {
                if (!_active)
                {
                    return;
                }
                string postFailure = _postFailure;
                if (postFailure != null)
                {
                    _postFailure = null;
                    SetStatus("指令檔事件轉交 UI 執行緒失敗，補檢查一次：" + postFailure);
                    Trigger();
                }
                EnsureWatcher();
            }
            catch (Exception ex)
            {
                SetStatus("建立指令檔監看失敗，5 秒後重試：" + ex.Message);
            }
        }

        // FileSystemWatcher 事件在背景執行緒：只負責轉交 UI 執行緒，不得丟出例外。
        private void OnWatcherEvent(object sender, FileSystemEventArgs e)
        {
            PostToUi(OnWatcherEventUi);
        }

        private void OnWatcherError(object sender, ErrorEventArgs e)
        {
            PostToUi(OnWatcherErrorUi);
        }

        // 背景執行緒丟出的例外會讓 ACT 結束，所以這裡把失敗記下來，由 UI 執行緒的監看 timer 回報並補跑一次。
        private void PostToUi(MethodInvoker action)
        {
            try
            {
                Control ui = _uiControl;
                if (_active && ui != null && !ui.IsDisposed && ui.IsHandleCreated)
                {
                    ui.BeginInvoke(action);
                }
            }
            catch (Exception ex)
            {
                _postFailure = ex.Message;
            }
        }

        private void OnWatcherEventUi()
        {
            try
            {
                Trigger();
            }
            catch (Exception ex)
            {
                SetStatus("內部錯誤（指令檔事件）：" + ex.Message);
            }
        }

        private void OnWatcherErrorUi()
        {
            try
            {
                if (!_active)
                {
                    return;
                }
                DisposeWatcher();
                SetStatus("指令檔監看中斷，重新建立");
                EnsureWatcher();
                Trigger();
            }
            catch (Exception ex)
            {
                SetStatus("重建指令檔監看失敗，5 秒後重試：" + ex.Message);
            }
        }

        // ---- 狀態顯示 ----
        // 只在 UI 執行緒呼叫；這裡若再丟例外，由 ACT 的 Application.ThreadException 接住，不會結束 ACT。

        private void SetStatus(string message)
        {
            string stamp = DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss", CultureInfo.InvariantCulture);
            string line = "[" + stamp + "] " + message;
            if (message == _lastMessage && _log.Count > 0)
            {
                _log[0] = line;
            }
            else
            {
                _log.Insert(0, line);
                if (_log.Count > MaxLogLines)
                {
                    _log.RemoveAt(_log.Count - 1);
                }
            }
            _lastMessage = message;

            if (_statusBox != null && !_statusBox.IsDisposed)
            {
                StringBuilder sb = new StringBuilder();
                sb.Append("XLM ACT Bridge").Append("\r\n");
                sb.Append("指令檔：").Append(_targetPath).Append("\r\n");
                sb.Append("本 ACT 判定區服：").Append(_regionDisplay).Append("\r\n");
                sb.Append("\r\n").Append("紀錄（新到舊）：").Append("\r\n");
                for (int i = 0; i < _log.Count; i++)
                {
                    sb.Append(_log[i]).Append("\r\n");
                }
                _statusBox.Text = sb.ToString();
            }
            if (_statusLabel != null && !_statusLabel.IsDisposed)
            {
                string shortText = "[" + _regionDisplay + "] " + message;
                if (shortText.Length > MaxLabelChars)
                {
                    shortText = shortText.Substring(0, MaxLabelChars) + "…";
                }
                _statusLabel.Text = shortText;
            }
        }
    }

    internal enum ReadStatus
    {
        Ok,
        NotFound,
        Busy,
        Rejected
    }

    internal enum OutcomeKind
    {
        Idle,
        Applied,
        Retry
    }

    internal sealed class TargetEntry
    {
        public uint Pid;
        public ulong Created;
    }

    internal sealed class TargetFile
    {
        public TargetEntry Tw;
        public TargetEntry Global;

        public TargetEntry Get(string regionKey)
        {
            if (regionKey == Bridge.RegionKeyTw)
            {
                return Tw;
            }
            if (regionKey == Bridge.RegionKeyGlobal)
            {
                return Global;
            }
            return null;
        }
    }

    internal sealed class FfxivControls
    {
        public ComboBox Region;
        public ComboBox ProcessId;
        public Button Refresh;
    }

    internal sealed class Outcome
    {
        public OutcomeKind Kind;
        public string Message;
        public string RegionDisplay;
        public string AppliedKey;
    }

    // 不依賴 ACT 執行環境的邏輯，便於單獨測試。
    internal static class Bridge
    {
        internal const string TargetFileName = "act_target.json";
        internal const int MaxFileBytes = 4096;
        internal const string GameProcessName = "ffxiv_dx11";
        internal const string FfxivPluginFileName = "FFXIV_ACT_Plugin.dll";
        internal const string RegionKeyGlobal = "global";
        internal const string RegionKeyTw = "tw";
        internal const string RegionTextGlobal = "Global";
        internal const string RegionTextTw = "\u7E41\u9AD4\u4E2D\u6587"; // 「繁體中文」，以跳脫碼避免原始碼編碼問題
        internal const string RegionComboName = "cboRegion";
        internal const string ProcessComboName = "cboProcessID";
        internal const string RefreshButtonName = "cmdReloadProcess";
        internal const string RefreshButtonText = "Refresh List";

        private static readonly UTF8Encoding StrictUtf8 = new UTF8Encoding(false, true);

        internal static string StateDirectory()
        {
            string appData = Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData);
            return Path.Combine(Path.Combine(appData, "xiv-launch-manager"), "state");
        }

        // ---- 讀檔 ----

        internal static ReadStatus ReadTargetText(string path, out string text, out string error)
        {
            text = null;
            error = null;
            byte[] buffer = new byte[MaxFileBytes + 1];
            int total = 0;
            try
            {
                // FileShare.Delete：讀取期間不擋 XLM 以 rename 取代本檔。
                using (FileStream stream = new FileStream(path, FileMode.Open, FileAccess.Read,
                    FileShare.ReadWrite | FileShare.Delete))
                {
                    if (stream.Length > MaxFileBytes)
                    {
                        error = "檔案大小 " + stream.Length + " bytes 超過上限 " + MaxFileBytes + " bytes";
                        return ReadStatus.Rejected;
                    }
                    int read;
                    while (total < buffer.Length && (read = stream.Read(buffer, total, buffer.Length - total)) > 0)
                    {
                        total += read;
                    }
                }
            }
            catch (FileNotFoundException)
            {
                return ReadStatus.NotFound;
            }
            catch (DirectoryNotFoundException)
            {
                return ReadStatus.NotFound;
            }
            catch (IOException ex)
            {
                // 共用違規等暫時狀況（XLM 正在取代檔案）。
                error = ex.Message;
                return ReadStatus.Busy;
            }
            catch (UnauthorizedAccessException ex)
            {
                // 檔案正被取代（刪除待決）時也可能暫時拒絕存取。
                error = ex.Message;
                return ReadStatus.Busy;
            }

            if (total > MaxFileBytes)
            {
                error = "檔案大小超過上限 " + MaxFileBytes + " bytes";
                return ReadStatus.Rejected;
            }
            if (total >= 3 && buffer[0] == 0xEF && buffer[1] == 0xBB && buffer[2] == 0xBF)
            {
                error = "檔案開頭有 UTF-8 BOM";
                return ReadStatus.Rejected;
            }
            try
            {
                text = StrictUtf8.GetString(buffer, 0, total);
            }
            catch (ArgumentException)
            {
                error = "不是有效的 UTF-8";
                return ReadStatus.Rejected;
            }
            return ReadStatus.Ok;
        }

        // ---- 嚴格解析 ----

        internal static TargetFile ParseTarget(string text, out string error)
        {
            error = null;
            if (text == null)
            {
                error = "內容為空";
                return null;
            }
            JsonReader reader = new JsonReader(text);
            TargetFile result = new TargetFile();
            bool seenTw = false;
            bool seenGlobal = false;
            try
            {
                reader.SkipWhitespace();
                reader.Expect('{', "頂層必須是 JSON 物件");
                reader.SkipWhitespace();
                if (reader.TryConsume('}'))
                {
                    reader.ExpectEnd();
                    return result;
                }
                while (true)
                {
                    reader.SkipWhitespace();
                    string key = reader.ReadString("頂層欄位名稱");
                    reader.SkipWhitespace();
                    reader.Expect(':', "欄位 \"" + key + "\" 後缺少 ':'");
                    reader.SkipWhitespace();
                    if (key == RegionKeyTw)
                    {
                        if (seenTw)
                        {
                            throw new FormatException("重複的欄位 \"tw\"");
                        }
                        seenTw = true;
                        result.Tw = ParseEntry(reader, key);
                    }
                    else if (key == RegionKeyGlobal)
                    {
                        if (seenGlobal)
                        {
                            throw new FormatException("重複的欄位 \"global\"");
                        }
                        seenGlobal = true;
                        result.Global = ParseEntry(reader, key);
                    }
                    else
                    {
                        throw new FormatException("不允許的頂層欄位 \"" + Printable(key) + "\"（只允許 tw、global）");
                    }
                    reader.SkipWhitespace();
                    if (reader.TryConsume(','))
                    {
                        continue;
                    }
                    reader.Expect('}', "頂層物件缺少 ',' 或 '}'");
                    break;
                }
                reader.ExpectEnd();
                return result;
            }
            catch (FormatException ex)
            {
                error = ex.Message + "（位置 " + reader.Position + "）";
                return null;
            }
        }

        private static TargetEntry ParseEntry(JsonReader reader, string regionKey)
        {
            reader.Expect('{', "\"" + regionKey + "\" 的值必須是物件");
            TargetEntry entry = new TargetEntry();
            bool seenPid = false;
            bool seenCreated = false;
            reader.SkipWhitespace();
            if (!reader.TryConsume('}'))
            {
                while (true)
                {
                    reader.SkipWhitespace();
                    string key = reader.ReadString("\"" + regionKey + "\" 內的欄位名稱");
                    reader.SkipWhitespace();
                    reader.Expect(':', "欄位 \"" + regionKey + "." + Printable(key) + "\" 後缺少 ':'");
                    reader.SkipWhitespace();
                    if (key == "pid")
                    {
                        if (seenPid)
                        {
                            throw new FormatException("\"" + regionKey + "\" 內重複的欄位 \"pid\"");
                        }
                        seenPid = true;
                        entry.Pid = (uint)reader.ReadUnsigned(regionKey + ".pid", uint.MaxValue);
                    }
                    else if (key == "created")
                    {
                        if (seenCreated)
                        {
                            throw new FormatException("\"" + regionKey + "\" 內重複的欄位 \"created\"");
                        }
                        seenCreated = true;
                        entry.Created = reader.ReadUnsigned(regionKey + ".created", ulong.MaxValue);
                    }
                    else
                    {
                        throw new FormatException("\"" + regionKey + "\" 內不允許的欄位 \"" + Printable(key) + "\"（只允許 pid、created）");
                    }
                    reader.SkipWhitespace();
                    if (reader.TryConsume(','))
                    {
                        continue;
                    }
                    reader.Expect('}', "\"" + regionKey + "\" 物件缺少 ',' 或 '}'");
                    break;
                }
            }
            if (!seenPid || !seenCreated)
            {
                throw new FormatException("\"" + regionKey + "\" 缺少欄位 " + (seenPid ? "created" : "pid"));
            }
            return entry;
        }

        private static string Printable(string value)
        {
            StringBuilder sb = new StringBuilder();
            int limit = Math.Min(value.Length, 32);
            for (int i = 0; i < limit; i++)
            {
                char c = value[i];
                sb.Append(char.IsControl(c) ? '?' : c);
            }
            if (value.Length > limit)
            {
                sb.Append("…");
            }
            return sb.ToString();
        }

        private sealed class JsonReader
        {
            private readonly string _text;
            private int _pos;

            public JsonReader(string text)
            {
                _text = text;
            }

            public int Position
            {
                get { return _pos; }
            }

            public void SkipWhitespace()
            {
                while (_pos < _text.Length)
                {
                    char c = _text[_pos];
                    if (c != ' ' && c != '\t' && c != '\n' && c != '\r')
                    {
                        break;
                    }
                    _pos++;
                }
            }

            public bool TryConsume(char c)
            {
                if (_pos < _text.Length && _text[_pos] == c)
                {
                    _pos++;
                    return true;
                }
                return false;
            }

            public void Expect(char c, string message)
            {
                if (!TryConsume(c))
                {
                    throw new FormatException(message);
                }
            }

            public void ExpectEnd()
            {
                SkipWhitespace();
                if (_pos != _text.Length)
                {
                    throw new FormatException("JSON 物件結束後還有多餘內容");
                }
            }

            public string ReadString(string what)
            {
                if (!TryConsume('"'))
                {
                    throw new FormatException(what + "必須是字串");
                }
                StringBuilder sb = new StringBuilder();
                while (true)
                {
                    if (_pos >= _text.Length)
                    {
                        throw new FormatException("字串沒有結尾");
                    }
                    char c = _text[_pos++];
                    if (c == '"')
                    {
                        return sb.ToString();
                    }
                    if (c < ' ')
                    {
                        throw new FormatException("字串含未跳脫的控制字元");
                    }
                    if (c != '\\')
                    {
                        sb.Append(c);
                        continue;
                    }
                    if (_pos >= _text.Length)
                    {
                        throw new FormatException("字串沒有結尾");
                    }
                    char esc = _text[_pos++];
                    switch (esc)
                    {
                        case '"': sb.Append('"'); break;
                        case '\\': sb.Append('\\'); break;
                        case '/': sb.Append('/'); break;
                        case 'b': sb.Append('\b'); break;
                        case 'f': sb.Append('\f'); break;
                        case 'n': sb.Append('\n'); break;
                        case 'r': sb.Append('\r'); break;
                        case 't': sb.Append('\t'); break;
                        case 'u':
                            if (_pos + 4 > _text.Length)
                            {
                                throw new FormatException("\\u 跳脫不完整");
                            }
                            int code;
                            if (!int.TryParse(_text.Substring(_pos, 4), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out code))
                            {
                                throw new FormatException("\\u 跳脫不是 4 位十六進位");
                            }
                            sb.Append((char)code);
                            _pos += 4;
                            break;
                        default:
                            throw new FormatException("無效的跳脫字元 \\" + esc);
                    }
                }
            }

            // 只接受非負整數：不接受負號、小數、指數、前導 0、字串或其他型別。
            public ulong ReadUnsigned(string field, ulong max)
            {
                int start = _pos;
                if (_pos >= _text.Length || _text[_pos] < '0' || _text[_pos] > '9')
                {
                    throw new FormatException("欄位 " + field + " 必須是非負整數");
                }
                ulong value = 0;
                while (_pos < _text.Length && _text[_pos] >= '0' && _text[_pos] <= '9')
                {
                    ulong digit = (ulong)(_text[_pos] - '0');
                    if (value > (max - digit) / 10)
                    {
                        throw new FormatException("欄位 " + field + " 超出範圍（上限 " + max.ToString(CultureInfo.InvariantCulture) + "）");
                    }
                    value = value * 10 + digit;
                    _pos++;
                }
                if (_pos - start > 1 && _text[start] == '0')
                {
                    throw new FormatException("欄位 " + field + " 不得有前導 0");
                }
                if (_pos < _text.Length && (_text[_pos] == '.' || _text[_pos] == 'e' || _text[_pos] == 'E'))
                {
                    throw new FormatException("欄位 " + field + " 必須是整數");
                }
                return value;
            }
        }

        // ---- 程序驗證 ----

        // 回傳 null 表示驗證通過；否則為不採用的原因。
        internal static string CheckProcess(uint pid, ulong created, string expectedName)
        {
            if (pid == 0 || pid > int.MaxValue)
            {
                return "pid " + pid.ToString(CultureInfo.InvariantCulture) + " 不是有效的程序 ID";
            }
            Process process;
            try
            {
                process = Process.GetProcessById((int)pid);
            }
            catch (ArgumentException)
            {
                return "pid " + pid.ToString(CultureInfo.InvariantCulture) + " 的程序不存在（遊戲可能已關閉）";
            }
            using (process)
            {
                string name;
                try
                {
                    name = process.ProcessName;
                }
                catch (InvalidOperationException)
                {
                    return "pid " + pid.ToString(CultureInfo.InvariantCulture) + " 的程序已結束";
                }
                if (!string.Equals(name, expectedName, StringComparison.OrdinalIgnoreCase))
                {
                    return "pid " + pid.ToString(CultureInfo.InvariantCulture) + " 的程序是 " + name + "，不是 " + expectedName;
                }
                long startFileTime;
                try
                {
                    startFileTime = process.StartTime.ToFileTimeUtc();
                }
                catch (Exception ex)
                {
                    return "無法讀取 pid " + pid.ToString(CultureInfo.InvariantCulture) + " 的啟動時間：" + ex.Message;
                }
                if (created > long.MaxValue || startFileTime != (long)created)
                {
                    return "pid " + pid.ToString(CultureInfo.InvariantCulture) + " 的啟動時間 "
                        + startFileTime.ToString(CultureInfo.InvariantCulture) + " 與指令檔 created "
                        + created.ToString(CultureInfo.InvariantCulture) + " 不符（指令檔過時或 pid 已被重用）";
                }
                return null;
            }
        }

        // ---- 區服 ----

        internal static string ComboText(ComboBox combo)
        {
            object item = combo.SelectedItem;
            return item == null ? null : item.ToString();
        }

        internal static string RegionKeyFromText(string text)
        {
            if (text == RegionTextGlobal)
            {
                return RegionKeyGlobal;
            }
            if (text == RegionTextTw)
            {
                return RegionKeyTw;
            }
            return null;
        }

        internal static string DescribeRegion(string regionKey, string comboText)
        {
            if (regionKey == RegionKeyGlobal)
            {
                return "國際服（global）";
            }
            if (regionKey == RegionKeyTw)
            {
                return "繁中服（tw）";
            }
            return "不連動（FFXIV 插件 Region：" + (comboText ?? "未選取") + "）";
        }

        // ---- FFXIV 插件畫面 ----

        // 在 FFXIV 插件的 TabPage 底下遞迴尋找 cboRegion、cboProcessID 與 Refresh List 按鈕；
        // 任一找不到就回傳 null，missing 列出缺少的控制項。
        internal static FfxivControls FindFfxivControls(Control root, out string missing)
        {
            ComboBox region = null;
            ComboBox processId = null;
            Button refreshByName = null;
            Button refreshByText = null;
            Stack<Control> pending = new Stack<Control>();
            pending.Push(root);
            while (pending.Count > 0)
            {
                Control control = pending.Pop();
                ComboBox combo = control as ComboBox;
                Button button = control as Button;
                if (combo != null)
                {
                    if (region == null && combo.Name == RegionComboName)
                    {
                        region = combo;
                    }
                    else if (processId == null && combo.Name == ProcessComboName)
                    {
                        processId = combo;
                    }
                }
                else if (button != null)
                {
                    if (refreshByName == null && button.Name == RefreshButtonName)
                    {
                        refreshByName = button;
                    }
                    if (refreshByText == null && button.Text == RefreshButtonText)
                    {
                        refreshByText = button;
                    }
                }
                foreach (Control child in control.Controls)
                {
                    pending.Push(child);
                }
            }
            Button refresh = refreshByName ?? refreshByText;
            List<string> absent = new List<string>();
            if (region == null)
            {
                absent.Add("ComboBox " + RegionComboName);
            }
            if (processId == null)
            {
                absent.Add("ComboBox " + ProcessComboName);
            }
            if (refresh == null)
            {
                absent.Add("Button \"" + RefreshButtonText + "\"");
            }
            if (absent.Count > 0)
            {
                missing = string.Join("、", absent.ToArray());
                return null;
            }
            missing = null;
            FfxivControls result = new FfxivControls();
            result.Region = region;
            result.ProcessId = processId;
            result.Refresh = refresh;
            return result;
        }

        // 按 Refresh List 後在 cboProcessID 選取 pid；清單裡沒有就回傳 false。
        internal static bool SelectProcessId(FfxivControls controls, uint pid, out string error)
        {
            ClickButton(controls.Refresh);
            string wanted = pid.ToString(CultureInfo.InvariantCulture);
            ComboBox combo = controls.ProcessId;
            for (int i = 0; i < combo.Items.Count; i++)
            {
                object item = combo.Items[i];
                if (item == null || item.ToString() != wanted)
                {
                    continue;
                }
                if (combo.SelectedIndex != i)
                {
                    combo.SelectedIndex = i;
                }
                if (ComboText(combo) != wanted)
                {
                    error = "選取 pid " + wanted + " 後 cboProcessID 顯示為 " + (ComboText(combo) ?? "未選取");
                    return false;
                }
                error = null;
                return true;
            }
            error = "pid " + wanted + " 不在 FFXIV Process ID 清單中（清單 " + combo.Items.Count + " 項）";
            return false;
        }

        // FFXIV 插件分頁沒顯示時按鈕不可選取，PerformClick 會直接略過；
        // 此時改以反射呼叫 Button.OnClick，效果與使用者按下相同。
        internal static void ClickButton(Button button)
        {
            if (button.CanSelect)
            {
                button.PerformClick();
                return;
            }
            MethodInfo onClick = typeof(Button).GetMethod("OnClick", BindingFlags.Instance | BindingFlags.NonPublic,
                null, new Type[] { typeof(EventArgs) }, null);
            if (onClick == null)
            {
                throw new MissingMethodException("Button", "OnClick");
            }
            try
            {
                onClick.Invoke(button, new object[] { EventArgs.Empty });
            }
            catch (TargetInvocationException ex)
            {
                // 反射會包一層；改丟出 FFXIV 插件 Click 處理本身的錯誤，狀態文字才看得出原因。
                Exception inner = ex.InnerException ?? ex;
                throw new InvalidOperationException("按下 Refresh List 時 FFXIV 插件發生錯誤：" + inner.Message, inner);
            }
        }

        // 依本 ACT 的 Region 取指令檔中對應項目，驗證程序後切換 Process ID。
        internal static Outcome ApplyTarget(TargetFile target, FfxivControls controls, string lastAppliedKey, string expectedProcessName)
        {
            Outcome outcome = new Outcome();
            outcome.Kind = OutcomeKind.Idle;
            string regionText = ComboText(controls.Region);
            string regionKey = RegionKeyFromText(regionText);
            outcome.RegionDisplay = DescribeRegion(regionKey, regionText);
            if (regionKey == null)
            {
                outcome.Message = "此 ACT 的區服不是國際或繁中，不連動（FFXIV 插件 Region：" + (regionText ?? "未選取") + "）";
                return outcome;
            }
            TargetEntry entry = target.Get(regionKey);
            if (entry == null)
            {
                outcome.Message = "指令檔沒有本區服（" + regionKey + "）的項目，不動作";
                return outcome;
            }
            string pidText = entry.Pid.ToString(CultureInfo.InvariantCulture);
            string key = regionKey + "|" + pidText + "|" + entry.Created.ToString(CultureInfo.InvariantCulture);
            if (key == lastAppliedKey)
            {
                outcome.Message = "本區服目標未變（pid " + pidText + "），已套用過，不重複套用";
                return outcome;
            }
            string processError = CheckProcess(entry.Pid, entry.Created, expectedProcessName);
            if (processError != null)
            {
                outcome.Message = "不套用：" + processError;
                return outcome;
            }
            string selectError;
            if (!SelectProcessId(controls, entry.Pid, out selectError))
            {
                outcome.Kind = OutcomeKind.Retry;
                outcome.Message = selectError;
                return outcome;
            }
            outcome.Kind = OutcomeKind.Applied;
            outcome.AppliedKey = key;
            outcome.Message = "已套用：FFXIV Process ID = " + pidText;
            return outcome;
        }
    }
}
