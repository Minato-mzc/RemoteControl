package com.remotecontrol.app.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.ContentPaste
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowLeft
import androidx.compose.material.icons.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.outlined.Keyboard
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.remotecontrol.app.net.Macro
import com.remotecontrol.app.net.Macros
import com.remotecontrol.app.net.VKey

/**
 * Floating keyboard entry. Collapsed → a small FAB in the bottom-right.
 * Expanded → a translucent panel above with:
 *   - a BasicTextField that pulls up the system IME (works for CJK/emoji)
 *   - a horizontal row of special keys (Esc, Tab, arrows, navigation, Win, F-keys)
 *
 * IME flow: while the user is composing (composition ≠ null) we keep the text
 * locally. As soon as composition ends — i.e. they accepted a CJK word, typed
 * an ASCII char, or hit space — we ship the whole text via `onKeyText` and
 * clear the field so it always represents the current composing word.
 */
@Composable
fun KeyboardOverlay(
    onKeyText: (String) -> Unit,
    onKeyTap: (vk: Int) -> Unit,
    onClipboardPush: (String) -> Unit,
    onClipboardPull: () -> Unit,
    onMacro: (Macro) -> Unit,
    modifier: Modifier = Modifier,
) {
    var expanded by remember { mutableStateOf(false) }
    // Persist field & committed state at this scope so closing/reopening the
    // panel doesn't reset the textField (and the diff baseline) — the user
    // can resume editing the same buffer that the PC already has.
    var fieldValue by remember { mutableStateOf(TextFieldValue("")) }
    var committed by remember { mutableStateOf("") }

    Box(modifier = modifier) {
        if (!expanded) {
            FloatingActionButton(
                onClick = { expanded = true },
                modifier = Modifier
                    .align(Alignment.BottomEnd)
                    .padding(16.dp),
                containerColor = MaterialTheme.colorScheme.primaryContainer,
            ) {
                Icon(Icons.Outlined.Keyboard, contentDescription = "显示键盘")
            }
        } else {
            ExpandedPanel(
                fieldValue = fieldValue,
                onFieldChange = { new ->
                    fieldValue = new
                    val newCommitted = stableTextOf(new)
                    syncDiff(committed, newCommitted, onKeyText) { onKeyTap(VKey.BACK) }
                    committed = newCommitted
                },
                onKeyTap = onKeyTap,
                onClipboardPush = onClipboardPush,
                onClipboardPull = onClipboardPull,
                onMacro = onMacro,
                onClose = { expanded = false },
                modifier = Modifier.align(Alignment.BottomCenter),
            )
        }
    }
}

@Composable
private fun ExpandedPanel(
    fieldValue: TextFieldValue,
    onFieldChange: (TextFieldValue) -> Unit,
    onKeyTap: (vk: Int) -> Unit,
    onClipboardPush: (String) -> Unit,
    onClipboardPull: () -> Unit,
    onMacro: (Macro) -> Unit,
    onClose: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val bg = Color(0xCC202020)
    Column(
        modifier = modifier
            .fillMaxWidth()
            .background(bg, RoundedCornerShape(topStart = 16.dp, topEnd = 16.dp))
            .padding(8.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            ImeRelay(
                value = fieldValue,
                onValueChange = onFieldChange,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = onClose) {
                Icon(Icons.Default.Close, contentDescription = "收起", tint = Color.White)
            }
        }

        // Single horizontally scrollable row of special keys.
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .horizontalScroll(rememberScrollState()),
            horizontalArrangement = Arrangement.spacedBy(6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            KKey("Esc") { onKeyTap(VKey.ESCAPE) }
            KKey("Tab") { onKeyTap(VKey.TAB) }
            KKey("⌫") { onKeyTap(VKey.BACK) }
            KKey("Enter") { onKeyTap(VKey.RETURN) }
            Spacer(Modifier.width(4.dp))
            KIcon(Icons.Default.KeyboardArrowUp) { onKeyTap(VKey.UP) }
            KIcon(Icons.Default.KeyboardArrowDown) { onKeyTap(VKey.DOWN) }
            KIcon(Icons.Default.KeyboardArrowLeft) { onKeyTap(VKey.LEFT) }
            KIcon(Icons.Default.KeyboardArrowRight) { onKeyTap(VKey.RIGHT) }
            Spacer(Modifier.width(4.dp))
            KKey("Home") { onKeyTap(VKey.HOME) }
            KKey("End") { onKeyTap(VKey.END) }
            KKey("PgUp") { onKeyTap(VKey.PRIOR) }
            KKey("PgDn") { onKeyTap(VKey.NEXT) }
            Spacer(Modifier.width(4.dp))
            KKey("Win") { onKeyTap(VKey.LWIN) }
            KKey("PrtSc") { onKeyTap(VKey.SNAPSHOT) }
            FKeyMenu(onKeyTap)
            Spacer(Modifier.width(4.dp))
            KIcon(Icons.Default.ContentCopy) {
                val text = readPhoneClipboard(context)
                if (text.isNotEmpty()) {
                    onClipboardPush(text)
                    Toast.makeText(context, "已推送到 PC", Toast.LENGTH_SHORT).show()
                } else {
                    Toast.makeText(context, "手机剪贴板为空", Toast.LENGTH_SHORT).show()
                }
            }
            KIcon(Icons.Default.ContentPaste) { onClipboardPull() }
        }

        // Macro row: common Windows shortcuts in one tap.
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .horizontalScroll(rememberScrollState()),
            horizontalArrangement = Arrangement.spacedBy(6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            for (macro in Macros.DEFAULTS) {
                KKey(macro.label) { onMacro(macro) }
            }
        }
    }
}

private fun readPhoneClipboard(context: Context): String {
    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
        ?: return ""
    val clip = cm.primaryClip ?: return ""
    if (clip.itemCount == 0) return ""
    return clip.getItemAt(0).coerceToText(context).toString()
}

fun writePhoneClipboard(context: Context, text: String) {
    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager ?: return
    cm.setPrimaryClip(ClipData.newPlainText("RemoteControl", text))
}

@Composable
private fun ImeRelay(
    value: TextFieldValue,
    onValueChange: (TextFieldValue) -> Unit,
    modifier: Modifier = Modifier,
) {
    val focusRequester = remember { FocusRequester() }
    LaunchedEffect(Unit) { focusRequester.requestFocus() }

    BasicTextField(
        value = value,
        onValueChange = onValueChange,
        textStyle = TextStyle(color = Color.White, fontSize = 16.sp),
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Default),
        modifier = modifier
            .background(Color(0x33FFFFFF), RoundedCornerShape(8.dp))
            .padding(horizontal = 12.dp, vertical = 8.dp)
            .height(40.dp)
            .focusRequester(focusRequester),
    )
}

/** Text outside the IME composition range — i.e. content that's been committed. */
private fun stableTextOf(v: TextFieldValue): String {
    val comp = v.composition ?: return v.text
    return v.text.substring(0, comp.start) + v.text.substring(comp.end)
}

/**
 * Mirror local edits to the PC by computing the longest common prefix and
 * sending only the delta as backspaces + new text. Handles append, delete,
 * and mid-string edits without retyping the whole field.
 */
private fun syncDiff(
    old: String,
    new: String,
    onText: (String) -> Unit,
    onBackspace: () -> Unit,
) {
    if (old == new) return
    when {
        new.startsWith(old) -> onText(new.substring(old.length))
        old.startsWith(new) -> repeat(old.length - new.length) { onBackspace() }
        else -> {
            val prefixLen = old.commonPrefixWith(new).length
            repeat(old.length - prefixLen) { onBackspace() }
            val tail = new.substring(prefixLen)
            if (tail.isNotEmpty()) onText(tail)
        }
    }
}

@Composable
private fun KKey(label: String, onClick: () -> Unit) {
    Button(
        onClick = onClick,
        contentPadding = PaddingValues(horizontal = 12.dp, vertical = 4.dp),
        colors = ButtonDefaults.buttonColors(
            containerColor = Color(0x33FFFFFF),
            contentColor = Color.White,
        ),
        modifier = Modifier.height(36.dp),
    ) { Text(label, fontSize = 13.sp) }
}

@Composable
private fun KIcon(icon: androidx.compose.ui.graphics.vector.ImageVector, onClick: () -> Unit) {
    IconButton(
        onClick = onClick,
        modifier = Modifier
            .height(36.dp)
            .width(36.dp)
            .background(Color(0x33FFFFFF), RoundedCornerShape(8.dp)),
    ) { Icon(icon, contentDescription = null, tint = Color.White) }
}

@Composable
private fun FKeyMenu(onKeyTap: (vk: Int) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        KKey("F-keys ▾") { open = true }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            for (n in 1..12) {
                DropdownMenuItem(
                    text = { Text("F$n") },
                    onClick = {
                        onKeyTap(VKey.f(n))
                        open = false
                    },
                )
            }
        }
    }
}
