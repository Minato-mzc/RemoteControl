package com.remotecontrol.app.ui

import android.content.Intent
import android.net.Uri
import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * Bottom-sheet listing files the PC has sent to this phone (M6 v2 inbound
 * direction). Tapping a row hands the file off to whatever system app
 * claims `ACTION_VIEW` for its MIME type via a `FileProvider` content URI.
 *
 * App-private external storage (`Android/data/<pkg>/files/Downloads/`)
 * is invisible to most user-facing file managers on Android 10+, so
 * giving users a way to reach it from inside the app is the difference
 * between "files arrived but I can't find them" and a useful feature.
 *
 * The list is read on each open (no live filesystem watching) — file
 * sends complete events trigger a toast, and the user reopens the sheet
 * to see new entries. Keeps the UI simple while we figure out whether
 * something more elaborate (search, delete, share to other apps) is
 * worth the maintenance.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ReceivedFilesSheet(
    downloadsDir: File,
    onDismiss: () -> Unit,
) {
    val context = LocalContext.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)

    var files by remember { mutableStateOf<List<File>>(emptyList()) }
    LaunchedEffect(downloadsDir) {
        files = if (downloadsDir.exists()) {
            downloadsDir.listFiles()
                ?.filter { it.isFile }
                ?.sortedByDescending { it.lastModified() }
                ?: emptyList()
        } else {
            emptyList()
        }
    }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 16.dp, vertical = 8.dp),
        ) {
            Text(
                "已接收的文件",
                fontSize = 18.sp,
                fontWeight = FontWeight.SemiBold,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                downloadsDir.absolutePath,
                fontSize = 11.sp,
                color = Color(0xFF999999),
                maxLines = 2,
            )
            Spacer(Modifier.height(12.dp))

            if (files.isEmpty()) {
                Text(
                    "暂无已接收的文件。文件会在 PC 端拖拽到二维码页面后自动保存到上面这个目录。",
                    fontSize = 13.sp,
                    color = Color(0xFF666666),
                )
                Spacer(Modifier.height(24.dp))
            } else {
                // Cap the LazyColumn height so the sheet doesn't try to
                // fill the entire screen on long lists — the sheet's own
                // gesture region still works above it.
                LazyColumn(
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(400.dp),
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    items(files, key = { it.absolutePath }) { f ->
                        FileRow(file = f) {
                            openFile(context, f) {
                                Toast.makeText(
                                    context,
                                    "无法打开该文件类型",
                                    Toast.LENGTH_SHORT,
                                ).show()
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun FileRow(file: File, onClick: () -> Unit) {
    val date = remember(file.lastModified()) {
        SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.getDefault())
            .format(Date(file.lastModified()))
    }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .background(Color(0xFFF5F5F5), RoundedCornerShape(10.dp))
            .padding(horizontal = 12.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Column(modifier = Modifier.weight(1f)) {
            Text(
                file.name,
                fontSize = 14.sp,
                fontWeight = FontWeight.Medium,
                maxLines = 1,
            )
            Spacer(Modifier.height(2.dp))
            Text(
                "${humanFileBytes(file.length())} · $date",
                fontSize = 11.sp,
                color = Color(0xFF888888),
            )
        }
        Text("打开", fontSize = 12.sp, color = Color(0xFF1f4d8b))
    }
}

private fun openFile(
    context: android.content.Context,
    file: File,
    onError: () -> Unit,
) {
    // FileProvider authority must match the `<provider>` entry in
    // AndroidManifest.xml. Using `${applicationId}.fileprovider`
    // keeps it stable across debug/release flavors.
    val authority = "${context.packageName}.fileprovider"
    val uri: Uri = try {
        FileProvider.getUriForFile(context, authority, file)
    } catch (e: IllegalArgumentException) {
        // File outside the paths whitelisted in file_paths.xml. Should
        // never happen for files inside `downloadsDir`, but bail
        // gracefully if it does.
        onError()
        return
    }
    val mime = mimeForName(file.name)
    val intent = Intent(Intent.ACTION_VIEW).apply {
        setDataAndType(uri, mime)
        // Required so the target app can read the content URI we just
        // granted it; without this it gets SecurityException on read.
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    }
    try {
        // Chooser so the user can pick which viewer (gallery vs video
        // player vs editor etc.) — gives consistent UX even when one
        // app sets itself as the default for a type.
        context.startActivity(Intent.createChooser(intent, "打开文件"))
    } catch (e: android.content.ActivityNotFoundException) {
        onError()
    }
}

/** Crude extension → MIME mapping for the file types most likely to
 *  show up in a PC→phone drop. Falls through to the wildcard
 *  `application/octet-stream` equivalent so the system chooser still
 *  surfaces every app willing to handle anything. */
private fun mimeForName(name: String): String {
    val ext = name.substringAfterLast('.', "").lowercase(Locale.ROOT)
    return when (ext) {
        "jpg", "jpeg", "png", "gif", "webp", "bmp" -> "image/*"
        "mp4", "mkv", "mov", "webm", "avi" -> "video/*"
        "mp3", "wav", "flac", "m4a", "ogg" -> "audio/*"
        "pdf" -> "application/pdf"
        "txt", "log", "md", "csv" -> "text/plain"
        "zip" -> "application/zip"
        "apk" -> "application/vnd.android.package-archive"
        else -> "*/*"
    }
}

private fun humanFileBytes(b: Long): String = when {
    b < 1024 -> "$b B"
    b < 1024L * 1024 -> "%.1f KB".format(b / 1024.0)
    b < 1024L * 1024 * 1024 -> "%.1f MB".format(b / 1024.0 / 1024.0)
    else -> "%.2f GB".format(b / 1024.0 / 1024.0 / 1024.0)
}
