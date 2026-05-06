# 开发环境状态

## 已装好
- **JDK 21 (Temurin)** —— 系统 JDK
- **JDK 21 (JBR)** —— Android Studio 自带，在 `D:\AndroidStudio\jbr`
- **Android SDK** —— `C:\Users\34630\AppData\Local\Android\Sdk`，含 platform-tools、build-tools 34/36/37
- **Android Studio** —— `D:\AndroidStudio`
- **Gradle 8.7** —— 用户本地缓存，项目 wrapper 已生成
- **Rust 1.95 (MSVC toolchain)** —— `C:\Users\34630\.cargo\bin`

## 正在装
- **Visual Studio 2022 Build Tools** —— 含 MSVC 编译器 + Windows 11 SDK，Rust 链接需要（后台静默安装）

---

## 构建 & 运行

### PC 端

等 MSVC Build Tools 装完后：

```bash
cd server-pc
cargo run --release
```

（首次编译会下很多依赖 + 编译一堆 crate，5-10 分钟）

### Android 端

已经配好 `local.properties`，Gradle wrapper 也生成了。两种方式：

**方式 A：Android Studio**
1. 打开 Studio → `Open` → 选 `D:\ClaudeCode\RemoteControl\app-android`
2. 等 Gradle sync 完成
3. 用 USB 连手机（开发者模式 + USB 调试），点 Run

**方式 B：命令行**
```bash
cd app-android
JAVA_HOME="D:/AndroidStudio/jbr" ./gradlew assembleDebug
# 安装到已连接的设备
JAVA_HOME="D:/AndroidStudio/jbr" ./gradlew installDebug
```

APK 输出在 `app/build/outputs/apk/debug/app-debug.apk`。

---

## 手机 Mate 70 Pro 调试准备

1. **设置** → **关于手机** → 连续点击"版本号" 7 次，开启开发者选项
2. **设置** → **系统和更新** → **开发人员选项** → 打开 **USB 调试**
3. 用数据线连电脑，手机会弹窗"允许 USB 调试吗"，勾"总是允许"后确定
4. 在 PC 终端跑 `adb devices`，应看到手机序列号（若看不到，尝试换线或换 USB 口）

---

## M1 验证连接

1. PC 端：`cd server-pc && cargo run --release`（终端打印二维码 + 6 位配对码）
2. 手机：打开 RemoteControl App → 扫码
3. 成功标志：
   - PC 日志：`handshake OK  peer=... session=...`
   - 手机界面：显示 "已连接" + 服务器名 + session ID

## 常见问题

- **`cargo build` 报 `link.exe not found`** —— MSVC Build Tools 还没装完，等就行
- **手机连不上 PC** —— 确认同一 Wi-Fi；Windows 防火墙首次弹窗要放行；若用手机热点，PC 的 IP 一般是 `192.168.137.1`
- **Android Studio Sync 慢** —— 首次会下 AGP、Compose、CameraX、ML Kit 等几百 MB，建议开代理或换国内镜像
