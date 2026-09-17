package io.github.overcuriousity.engram.core.db

import android.content.Context
import android.database.sqlite.SQLiteDatabase
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Version 1 holds the outbox, and the outbox is the one thing on the device
 * that exists nowhere else. A phone that installed the first release and
 * updates to this one must keep what it owes.
 *
 * The version-1 file is built from the SQL in `schemas/…/1.json`, by hand, the
 * way the first release's Room built it. Opening it through [Db.open] then runs
 * the real migration, and Room checks the result against version 2's expected
 * schema — so a typo in the hand-written migration fails here, not on a phone.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class MigrationTest {
    @Test fun aQueuedCaptureSurvivesTheUpdate() = runTest {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val file = context.getDatabasePath("engram.db").apply { parentFile?.mkdirs() }
        SQLiteDatabase.openOrCreateDatabase(file, null).use { v1 ->
            v1.execSQL("CREATE TABLE IF NOT EXISTS `outbox` (`id` TEXT NOT NULL, `kind` TEXT NOT NULL, `payload` TEXT NOT NULL, `createdAt` INTEGER NOT NULL, `attempts` INTEGER NOT NULL, `nextAt` INTEGER NOT NULL, `state` TEXT NOT NULL, `status` INTEGER, `answer` TEXT, `error` TEXT, PRIMARY KEY(`id`))")
            v1.execSQL("CREATE TABLE IF NOT EXISTS `outbox_files` (`outboxId` TEXT NOT NULL, `path` TEXT NOT NULL, `name` TEXT NOT NULL, `mime` TEXT NOT NULL, PRIMARY KEY(`outboxId`, `path`))")
            v1.execSQL("CREATE TABLE IF NOT EXISTS `moments` (`id` TEXT NOT NULL, `title` TEXT NOT NULL, `at` INTEGER NOT NULL, `fetchedAt` INTEGER NOT NULL, PRIMARY KEY(`id`))")
            v1.execSQL("CREATE TABLE IF NOT EXISTS room_master_table (id INTEGER PRIMARY KEY,identity_hash TEXT)")
            v1.execSQL("INSERT OR REPLACE INTO room_master_table (id,identity_hash) VALUES(42, '5ea49801bae4945870c4456202d4b639')")
            v1.execSQL("INSERT INTO outbox (id, kind, payload, createdAt, attempts, nextAt, state) VALUES ('o1', 'capture_text', '{\"text\":\"owed\"}', 1, 0, 1, 'queued')")
            v1.version = 1
        }

        val db = Db.open(context)
        try {
            val row = db.outboxDao().all().first().single()
            assertEquals("o1", row.id)
            assertEquals(State.queued, row.state)
            // And the new tables are there to be written.
            db.cacheDao().put(CacheRow("/k", "https://o", null, "{}", 1))
            assertEquals(1, db.cacheDao().count())
        } finally {
            db.close()
        }
    }
}
