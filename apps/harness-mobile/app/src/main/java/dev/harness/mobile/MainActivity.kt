package dev.harness.mobile

import android.annotation.SuppressLint
import android.content.Intent
import android.net.Uri
import android.net.http.SslError
import android.os.Bundle
import android.text.Html
import android.util.Log
import android.webkit.CookieManager
import android.webkit.SslErrorHandler
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Toast
import androidx.activity.OnBackPressedCallback
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.swiperefreshlayout.widget.SwipeRefreshLayout

/**
 * Full-screen client for the paired Harness origin. Only https URLs on the paired host load inside;
 * everything else is handed to the system browser. The server renders the UI, so this class only has
 * to keep the pairing alive and the WebView sane.
 */
class MainActivity : AppCompatActivity() {

    companion object {
        private const val TAG = "HarnessMain"

        /** Full pair link (`https://host/p#t=…`) to open once on first launch after pairing. */
        const val EXTRA_PAIR_URL = "dev.harness.mobile.PAIR_URL"

        private const val RETRY_URL = "harness-retry:reload"
        private const val REPAIR_URL = "harness-repair:forget"
    }

    private lateinit var web: WebView
    private lateinit var swipe: SwipeRefreshLayout

    private var origin: String = ""
    private var host: String = ""
    private var pendingUrl: String? = null
    private var showingError = false

    /** Pending `<input type=file>` callback; must always be answered (null = cancelled). */
    private var fileCallback: ValueCallback<Array<Uri>>? = null

    private val pickFiles =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            fileCallback?.onReceiveValue(pickedUris(result.resultCode, result.data))
            fileCallback = null
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val saved = PairStore.origin(this)
        if (saved == null) {
            startPairing()
            return
        }
        origin = saved
        host = Uri.parse(saved).host.orEmpty()

        setContentView(R.layout.activity_main)
        web = findViewById(R.id.web)
        swipe = findViewById(R.id.swipe)

        if (savedInstanceState == null) {
            pendingUrl = PairStore.parse(intent.getStringExtra(EXTRA_PAIR_URL))?.url
        }

        setUpWebView()
        setUpSwipe()
        setUpBack()

        if (savedInstanceState == null) {
            loadStart()
        }
    }

    @SuppressLint("SetJavaScriptEnabled")
    private fun setUpWebView() {
        val settings = web.settings
        settings.javaScriptEnabled = true
        settings.domStorageEnabled = true
        settings.allowFileAccess = false
        settings.allowContentAccess = false
        settings.javaScriptCanOpenWindowsAutomatically = false
        settings.setSupportMultipleWindows(false)
        settings.mediaPlaybackRequiresUserGesture = true
        settings.userAgentString = "${settings.userAgentString} HarnessMobile/${BuildConfig.VERSION_NAME}"

        web.setBackgroundColor(getColor(R.color.harness_bg))

        CookieManager.getInstance().apply {
            setAcceptCookie(true)
            setAcceptThirdPartyCookies(web, false)
        }
        WebView.setWebContentsDebuggingEnabled(BuildConfig.DEBUG)

        // 修复：长按总是弹出 Reload/Re-pair 菜单，挡住了系统的选择文字/复制。
        // 正常页面把长按交给 WebView（选择、复制、分享）；只有离线页才弹菜单。
        web.isLongClickable = true
        web.setOnLongClickListener {
            if (!showingError) return@setOnLongClickListener false
            showMenu()
            true
        }
        web.webViewClient = HarnessWebViewClient()
        // 修复：没有 WebChromeClient 时 <input type=file> 不会打开文件选择器，“+”按钮无反应。
        web.webChromeClient = object : WebChromeClient() {
            override fun onShowFileChooser(
                view: WebView,
                callback: ValueCallback<Array<Uri>>,
                params: FileChooserParams,
            ): Boolean {
                fileCallback?.onReceiveValue(null)
                fileCallback = callback
                val multiple = params.mode == FileChooserParams.MODE_OPEN_MULTIPLE
                // 修复：“Photos”和“Files”都打开相册且还要先选应用：图片直接开系统照片选择器，
                // 其他直接开文件管理器，不再弹 createChooser。
                val intent = if (onlyImages(params.acceptTypes)) {
                    photoIntent(multiple)
                } else {
                    filesIntent(multiple)
                }
                return try {
                    pickFiles.launch(intent)
                    true
                } catch (e: Exception) {
                    Log.w(TAG, "no file picker: ${e.javaClass.simpleName}")
                    fileCallback = null
                    false
                }
            }
        }
    }

    private fun setUpSwipe() {
        swipe.setColorSchemeColors(getColor(R.color.harness_accent))
        // 修复：聊天内容在页面内部的容器里滚动，web.scrollY 永远是 0，往上滑就会误触发刷新。
        // 只在离线错误页启用下拉刷新；正常页面用长按菜单里的 Reload。
        swipe.isEnabled = false
        swipe.setOnChildScrollUpCallback { _, _ -> !showingError || web.scrollY > 0 }
        swipe.setOnRefreshListener {
            if (showingError) {
                loadStart()
            } else {
                web.reload()
            }
        }
    }

    private fun setUpBack() {
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                if (showingError) {
                    showingError = false
                    finish()
                    return
                }
                if (web.canGoBack()) {
                    web.goBack()
                } else {
                    isEnabled = false
                    onBackPressedDispatcher.onBackPressed()
                }
            }
        })
    }

    /** Loads the pending pair link once, then the stored origin. */
    private fun loadStart() {
        showingError = false
        val url = pendingUrl ?: "$origin/"
        pendingUrl = null
        web.loadUrl(url)
    }

    private fun startPairing() {
        startActivity(Intent(this, PairingActivity::class.java))
        finish()
    }

    private fun showMenu() {
        if (isFinishing) return
        val items = arrayOf(getString(R.string.menu_reload), getString(R.string.menu_repair))
        AlertDialog.Builder(this)
            .setItems(items) { _, which ->
                when (which) {
                    0 -> loadStart()
                    else -> confirmRepair()
                }
            }
            .show()
    }

    private fun confirmRepair() {
        AlertDialog.Builder(this)
            .setMessage(R.string.repair_confirm)
            .setNegativeButton(R.string.menu_cancel, null)
            .setPositiveButton(R.string.repair_confirm_yes) { _, _ -> repair() }
            .show()
    }

    /** Forget the device: drop the stored origin, every cookie and all WebView storage. */
    private fun repair() {
        CookieManager.getInstance().removeAllCookies(null)
        CookieManager.getInstance().flush()
        runCatching { WebView(this).clearCache(true) }
        PairStore.clear(this)
        startPairing()
    }

    private fun showOffline(description: String) {
        if (showingError) return
        showingError = true
        swipe.isRefreshing = false
        swipe.isEnabled = true
        val detail = Html.escapeHtml(description.ifBlank { "unreachable" })
        val page = """
            <!doctype html>
            <html><head><meta charset="utf-8">
            <meta name="viewport" content="width=device-width, initial-scale=1">
            <title>Offline</title>
            <style>
              html,body{height:100%;margin:0;background:#0b0c0e;color:#edeef0;
                font:15px/1.5 Inter,system-ui,-apple-system,Segoe UI,Roboto,sans-serif}
              .wrap{min-height:100%;display:flex;flex-direction:column;align-items:center;
                justify-content:center;gap:14px;padding:24px;box-sizing:border-box;text-align:center}
              h1{font-size:22px;font-weight:600;margin:0}
              p{color:#8a909a;margin:0;max-width:32ch}
              small{color:#6b717c}
              a{display:inline-block;margin-top:10px;padding:12px 28px;border-radius:10px;
                background:linear-gradient(135deg,#7c8cff,#b07cff);color:#fff;font-weight:600;
                text-decoration:none}
              a.ghost{margin-top:0;background:none;color:#8a909a;font-weight:500}
            </style></head>
            <body><div class="wrap">
              <h1>Can't reach Harness</h1>
              <p>${Html.escapeHtml(origin)}<br><small>$detail</small></p>
              <p>Check that the desktop app is running and the tunnel is up.</p>
              <a href="$RETRY_URL">Retry</a>
              <a class="ghost" href="$REPAIR_URL">Re-pair this phone</a>
            </div></body></html>
        """.trimIndent()
        web.loadDataWithBaseURL(null, page, "text/html", "utf-8", null)
    }

    private fun openExternally(uri: Uri) {
        try {
            startActivity(
                Intent(Intent.ACTION_VIEW, uri).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        } catch (e: Exception) {
            Log.w(TAG, "no handler for ${uri.scheme}")
            Toast.makeText(this, "Can't open ${uri.host ?: uri}", Toast.LENGTH_SHORT).show()
        }
    }

    /** @return true when the navigation was consumed by us. */
    private fun handleUrl(uri: Uri): Boolean {
        if (uri.scheme.orEmpty().equals("harness-retry", ignoreCase = true)) {
            loadStart()
            return true
        }
        if (uri.scheme.orEmpty().equals("harness-repair", ignoreCase = true)) {
            confirmRepair()
            return true
        }
        val scheme = uri.scheme.orEmpty().lowercase()
        val sameHost = scheme == "https" && host.isNotEmpty() && uri.host.equals(host, ignoreCase = true)
        if (sameHost) return false
        openExternally(uri)
        return true
    }

    private inner class HarnessWebViewClient : WebViewClient() {

        override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean =
            handleUrl(request.url)

        @Suppress("DEPRECATION", "OVERRIDE_DEPRECATION")
        override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean =
            handleUrl(Uri.parse(url))

        override fun onPageStarted(view: WebView, url: String?, favicon: android.graphics.Bitmap?) {
            if (url?.startsWith("https://", ignoreCase = true) != true) return
            showingError = false
            swipe.isEnabled = false
            swipe.isRefreshing = true
        }

        override fun onPageFinished(view: WebView, url: String?) {
            swipe.isRefreshing = false
        }

        override fun onReceivedError(
            view: WebView,
            request: WebResourceRequest,
            error: WebResourceError,
        ) {
            if (!request.isForMainFrame) return
            Log.w(TAG, "main frame error ${error.errorCode}")
            showOffline(error.description?.toString().orEmpty())
        }

        override fun onReceivedHttpError(
            view: WebView,
            request: WebResourceRequest,
            errorResponse: WebResourceResponse,
        ) {
            if (!request.isForMainFrame || errorResponse.statusCode < 400) return
            Log.w(TAG, "main frame http ${errorResponse.statusCode}")
            showOffline("The tunnel answered HTTP ${errorResponse.statusCode}")
        }

        override fun onReceivedSslError(view: WebView, handler: SslErrorHandler, error: SslError) {
            // Never proceed past a certificate error.
            handler.cancel()
            showOffline("TLS handshake failed")
        }
    }

    override fun onPause() {
        CookieManager.getInstance().flush()
        if (::web.isInitialized) web.onPause()
        super.onPause()
    }

    override fun onResume() {
        super.onResume()
        if (::web.isInitialized) web.onResume()
    }

    override fun onDestroy() {
        if (::web.isInitialized) {
            web.setOnLongClickListener(null)
            web.destroy()
        }
        super.onDestroy()
    }
}
