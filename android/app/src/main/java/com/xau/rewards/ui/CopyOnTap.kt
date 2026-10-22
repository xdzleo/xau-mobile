package com.xau.rewards.ui

import android.os.Build
import android.widget.Toast
import androidx.compose.foundation.clickable
import androidx.compose.ui.Modifier
import androidx.compose.ui.composed
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString

/**
 * A tap copies [text] to the clipboard. Android 13 and later confirm a
 * copy on their own; older versions get a toast saying what was copied.
 */
fun Modifier.copyOnTap(text: String, what: String = "E-mail"): Modifier = composed {
    val clipboard = LocalClipboardManager.current
    val ctx = LocalContext.current
    clickable {
        clipboard.setText(AnnotatedString(text))
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
            Toast.makeText(ctx, "$what copiado", Toast.LENGTH_SHORT).show()
        }
    }
}
