package com.xau.rewards.ui

import android.content.Intent
import android.os.Build
import android.provider.Settings
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.app.NotificationManagerCompat
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.xau.rewards.core.Prefs
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau

/**
 * Configurações: the farm's notifications, and what this build is.
 *
 * Every switch writes straight to [Prefs]; the farm's service reads them
 * on each update, so there is no "save" and a running farm follows along.
 */
@Composable
fun SettingsScreen(
    onBack: () -> Unit,
    singleActive: Boolean = false,
    onSingleActive: (Boolean) -> Unit = {},
) {
    val c = Xau.colors
    val context = LocalContext.current
    val prefs = remember { Prefs(context) }
    var ongoing by remember { mutableStateOf(prefs.ongoingNotification) }
    var whenDone by remember { mutableStateOf(prefs.notifyWhenDone) }
    var levelUp by remember { mutableStateOf(prefs.notifyLevelUp) }
    // Re-read on resume: the user may have just allowed or blocked
    // notifications in the system settings this screen sent them to.
    var allowed by remember { mutableStateOf(true) }
    LifecycleResumeEffect(Unit) {
        allowed = NotificationManagerCompat.from(context).areNotificationsEnabled()
        onPauseOrDispose {}
    }

    BackHandler(onBack = onBack)

    Column(
        Modifier
            .fillMaxSize()
            .background(c.ink)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        Row(
            Modifier.fillMaxWidth().padding(Space.gutter),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                Modifier.size(Space.touch).clip(RoundedCornerShape(Radius.pill)).clickable(onClick = onBack),
                contentAlignment = Alignment.Center,
            ) { Text("‹", style = Xau.type.hero, color = c.textPrimary) }
            Spacer(Modifier.width(Space.sm))
            Text("Ajustes", style = Xau.type.section, color = c.textPrimary)
        }

        Column(
            Modifier
                .weight(1f)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = Space.gutter),
            verticalArrangement = Arrangement.spacedBy(Space.lg),
        ) {
            Box(Modifier.entrance(0)) {
                Group("Notificações") {
                    if (!allowed) {
                        Blocked {
                            context.startActivity(
                                Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                                    .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName),
                            )
                        }
                    }
                    Toggle(
                        title = "Notificação fixa do farm",
                        detail = "Mostra a conta, o progresso e o tempo restante enquanto o farm roda. " +
                            "Desligada, o Android ainda exige um aviso mínimo em segundo plano — " +
                            "recolhido e sem ícone na barra.",
                        checked = ongoing,
                    ) {
                        ongoing = it
                        prefs.ongoingNotification = it
                    }
                    Divider()
                    Toggle(
                        title = "Avisar quando o farm terminar",
                        detail = "Uma notificação quando uma conta completa os 15 minutos nos 8 jogos.",
                        checked = whenDone,
                    ) {
                        whenDone = it
                        prefs.notifyWhenDone = it
                    }
                    Divider()
                    Toggle(
                        title = "Avisar quando subir de nível",
                        detail = "Durante o farm, uma notificação assim que a conta sobe de nível no Rewards.",
                        checked = levelUp,
                    ) {
                        levelUp = it
                        prefs.notifyLevelUp = it
                    }
                }
            }

            Box(Modifier.entrance(1)) {
                Group("Contas") {
                    Toggle(
                        title = "Uma conta ativa por vez",
                        detail = "Só uma conta fica descongelada. Descongelar outra congela as demais " +
                            "na hora e para o farm delas.",
                        checked = singleActive,
                        onChange = onSingleActive,
                    )
                }
            }

            Box(Modifier.entrance(2)) {
                Group("Sobre") {
                    Info("Versão", appVersion(context))
                    Divider()
                    Info("Android", "${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})")
                }
            }

            Spacer(Modifier.height(Space.xxl))
        }
    }
}

@Composable
private fun Group(title: String, content: @Composable () -> Unit) {
    val c = Xau.colors
    Column {
        SectionLabel(title, Modifier.padding(start = Space.xs, bottom = Space.sm))
        Column(
            Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(Radius.card))
                .background(c.surface)
                .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
                .padding(vertical = Space.xs),
        ) { content() }
    }
}

@Composable
private fun Toggle(title: String, detail: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    val c = Xau.colors
    Row(
        Modifier
            .fillMaxWidth()
            .clickable { onChange(!checked) }
            .padding(horizontal = Space.lg, vertical = Space.md),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = Xau.type.rowTitle, color = c.textPrimary)
            Spacer(Modifier.height(2.dp))
            Text(detail, style = Xau.type.caption, color = c.textSecondary)
        }
        Spacer(Modifier.width(Space.md))
        Switch(
            checked = checked,
            onCheckedChange = onChange,
            colors = SwitchDefaults.colors(
                checkedThumbColor = Color.White,
                checkedTrackColor = c.green,
                checkedBorderColor = c.green,
                uncheckedThumbColor = c.textMuted,
                uncheckedTrackColor = c.surfaceHigh,
                uncheckedBorderColor = c.outline,
            ),
        )
    }
}

@Composable
private fun Info(label: String, value: String) {
    val c = Xau.colors
    Row(
        Modifier.fillMaxWidth().padding(horizontal = Space.lg, vertical = Space.md),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, style = Xau.type.body, color = c.textSecondary, modifier = Modifier.weight(1f))
        Text(value, style = Xau.type.body, color = c.textPrimary)
    }
}

/** Notifications are blocked system-wide for the app: say so, and fix it. */
@Composable
private fun Blocked(onOpen: () -> Unit) {
    val c = Xau.colors
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = Space.md, vertical = Space.sm)
            .clip(RoundedCornerShape(Radius.control))
            .background(c.danger.copy(alpha = 0.12f))
            .clickable(onClick = onOpen)
            .padding(Space.md),
    ) {
        Text("Notificações bloqueadas", style = Xau.type.rowTitle, color = c.danger)
        Text(
            "O Android não está deixando o app notificar. Toque para permitir.",
            style = Xau.type.caption,
            color = c.textSecondary,
        )
    }
}

@Composable
private fun Divider() {
    Box(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = Space.lg)
            .height(1.dp)
            .background(Xau.colors.outline),
    )
}

private fun appVersion(context: android.content.Context): String =
    runCatching { context.packageManager.getPackageInfo(context.packageName, 0).versionName }
        .getOrNull() ?: "—"
