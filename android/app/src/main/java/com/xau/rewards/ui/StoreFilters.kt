package com.xau.rewards.ui

import android.content.Context
import com.xau.rewards.core.StoreProduct
import org.json.JSONArray
import org.json.JSONObject

/**
 * The games the user starred in the Store. They belong to the phone, not
 * to an account: every profile opens the store to the same list.
 */
object StoreFavorites {
    private const val FILE = "xau_store"
    private const val KEY = "favorites"

    fun load(ctx: Context): List<StoreProduct> = runCatching {
        val raw = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE).getString(KEY, null)
            ?: return emptyList()
        val a = JSONArray(raw)
        (0 until a.length()).map { i ->
            val o = a.getJSONObject(i)
            StoreProduct(
                productId = o.getString("id"),
                title = o.getString("title"),
                publisher = o.optString("publisher").ifEmpty { null },
                image = o.optString("image").ifEmpty { null },
                price = o.optString("price").ifEmpty { null },
                was = o.optString("was").ifEmpty { null },
                pass = o.optString("pass").ifEmpty { null },
                rating = if (o.has("rating")) o.getDouble("rating") else null,
            )
        }
    }.getOrDefault(emptyList())

    fun save(ctx: Context, list: List<StoreProduct>) {
        val a = JSONArray()
        for (p in list) {
            a.put(
                JSONObject()
                    .put("id", p.productId)
                    .put("title", p.title)
                    .put("publisher", p.publisher ?: "")
                    .put("image", p.image ?: "")
                    .put("price", p.price ?: "")
                    .put("was", p.was ?: "")
                    .put("pass", p.pass ?: "")
                    .apply { p.rating?.let { put("rating", it) } },
            )
        }
        ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE).edit().putString(KEY, a.toString()).apply()
    }
}

/** What the store list is narrowed to. */
data class StoreFilter(
    val microsoftOnly: Boolean = false,
    val hideFree: Boolean = false,
    val minPrice: String = "",
    val maxPrice: String = "",
) {
    val active get() = microsoftOnly || hideFree || minPrice.isNotBlank() || maxPrice.isNotBlank()

    /** Free games are off the table, so the paid list is the one to read. */
    val wantsPaid get() = hideFree || (parseMoney(minPrice) ?: 0.0) > 0.0

    val min get() = parseMoney(minPrice)
    val max get() = parseMoney(maxPrice)
}

/**
 * What the user typed in a price box → reais; blank or junk → null. The
 * Store's own prices are read, and the filter applied, on the native side.
 */
fun parseMoney(text: String): Double? {
    val digits = text.filter { it.isDigit() || it == ',' || it == '.' }
    if (digits.none { it.isDigit() }) return null
    val normal = if (digits.contains(',')) digits.replace(".", "").replace(',', '.') else digits
    return normal.toDoubleOrNull()
}
