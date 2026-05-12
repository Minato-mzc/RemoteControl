# 发布构建 (Release / 分发)

把 PC 服务端 + 手机端打包成可以发给别人的 installer + APK 的完整流程。

---

## TL;DR

```powershell
# 一条命令搞定：PC + 安装器 + 安卓 APK
pwsh installer/build-installer.ps1
```

产物落在 `dist/`：

| 文件 | 说明 |
|---|---|
| `RemoteControl-Setup-X.Y.Z.exe` | Windows 安装器（含 WebView2 自动安装、开机自启选项、卸载器） |
| `RemoteControl-Server-X.Y.Z.exe` | 独立 PC 服务端（不想用安装器的人可以直接跑） |
| `RemoteControl-X.Y.Z.apk` | 已签名的 Release APK，直接给手机装 |

---

## 一次性准备

### 工具

- Rust + LLVM + Visual Studio 2022 Build Tools — PC 服务端编译已经依赖的，[`docs/SETUP.md`](SETUP.md) 里都有
- **Inno Setup 6+** — 从 https://jrsoftware.org/isdl.php 下载安装，默认装到 `C:\Program Files (x86)\Inno Setup 6\`
- Android SDK + JDK 21 — 已经装好

### Android 签名 keystore（**只生成一次**）

Release APK 必须用一个稳定的 keystore 签名——同一 `applicationId` 一旦发布过，后续更新就**只能用同一份 keystore** 签名，否则手机会拒绝安装（视为不同应用）。

1. **首次生成**：
   ```bash
   cd app-android
   keytool -genkeypair \
     -keystore release.jks \
     -alias release \
     -keyalg RSA -keysize 2048 \
     -validity 10950 \
     -storepass <你的密码> \
     -keypass <你的密码> \
     -dname "CN=RemoteControl,OU=Self,O=RemoteControl,L=Local,ST=Local,C=CN"
   ```

2. **配置密码**：在 `app-android/keystore.properties` 写入：
   ```properties
   storeFile=release.jks
   storePassword=<你的密码>
   keyAlias=release
   keyPassword=<你的密码>
   ```

3. **备份**：把 `release.jks` + `keystore.properties` 复制到至少一个 PC 之外的位置（U 盘 / 网盘加密目录）。**两个文件都已经被 `.gitignore` 排除**，绝对不要 commit。

   丢了的后果：当前 `applicationId` 没法再发更新，老用户只能卸载重装。

---

## 单步运行

`build-installer.ps1` 支持跳过部分阶段，调试时方便：

```powershell
pwsh installer/build-installer.ps1 -SkipAndroid      # 只构建 PC + 安装器
pwsh installer/build-installer.ps1 -SkipPcBuild      # 只构建 APK
pwsh installer/build-installer.ps1 -SkipInstaller    # 出独立 exe + APK，不打安装包
```

---

## 安装器做了什么

- 装到 `C:\Program Files\RemoteControl\`（需要管理员权限）
- 开始菜单 + 桌面快捷方式（桌面可选）
- 开机自启复选框（默认勾，写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`）
- 检测 WebView2 运行时，缺失则静默跑 `MicrosoftEdgeWebview2Setup.exe /silent /install`
- 卸载时先 `taskkill /F /IM remotecontrol-server.exe` 防止 exe 被占用

---

## SmartScreen 警告

没有买 EV 代码签名证书，所以接收方运行 installer 时 Windows 会弹"未识别的发布者"。引导话术：

> 出现"Windows 已保护你的电脑"蓝色弹窗 → 点击"**更多信息**" → 点"**仍要运行**"。

签了证书后这个就没了，但一年几百到几千刀，自用没必要。

---

## 版本号

`server-pc/Cargo.toml` 的 `version` 是真源——build 脚本会读出来塞进：

- `.exe` 的 VS_VERSIONINFO（Windows 资源管理器右键属性能看到）
- Installer 文件名
- `dist/*.exe` 文件名

**手机端** 的 `versionCode` / `versionName` 在 `app-android/app/build.gradle.kts`，要手动改。Bump 流程：

1. `server-pc/Cargo.toml` 改 `version`
2. `app-android/app/build.gradle.kts` 改 `versionCode`（+1） 和 `versionName`（matching string）
3. `pwsh installer/build-installer.ps1`

---

## 日志位置（用户机器）

Release build 是 GUI subsystem，启动**没有控制台窗口**，所有日志写到：

```
%LOCALAPPDATA%\RemoteControl\logs\server.log.YYYY-MM-DD
```

托盘右键 → "📂 打开日志文件夹" 直达。Crash 也会被 panic hook 捕获写进同一个文件。

debug 编译（`cargo run`）保留控制台 + stdout 输出。
