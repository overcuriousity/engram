package io.github.overcuriousity.engram.core.db

import android.content.Context
import androidx.room.Dao
import androidx.room.Database
import androidx.room.Entity
import androidx.room.Insert
import androidx.room.PrimaryKey
import androidx.room.Query
import androidx.room.Room
import androidx.room.RoomDatabase
import androidx.room.Transaction
import androidx.room.TypeConverter
import androidx.room.TypeConverters
import androidx.room.Update
import androidx.room.Upsert
import androidx.room.migration.Migration
import androidx.sqlite.db.SupportSQLiteDatabase
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.Flow

/**
 * What a row owes the server. The judging answers are here for the same reason
 * a capture is: a decision made on a train is a decision, and the queue is the
 * one place that knows what is owed — and the one place an answer can be taken
 * back out of before it goes.
 */
enum class Kind {
    capture_text,
    capture_files,
    done,
    snooze,
    pair_supersede,
    pair_synthesize,
    pair_discard,
    pair_dismiss,
    gap_dismiss,
    gap_forget,
    artifact_op,
    merge_undo,
    corpus_resolve,
    /** The one decision with no undo: `DELETE /artifacts/{id}`. Its own kind, never an `op`. */
    artifact_delete,
    /**
     * Any other write the device owes, named by its route: `{method, path,
     * body, label}`. The kinds above each carry a meaning the queue screen
     * words; this one carries its wording in the row, so the doors the web
     * grew after them — dating a reminder, re-reading a passage, clearing a
     * flag — need no new word in this column and no new database version.
     */
    call,
}

enum class State { queued, sent, refused, held }

/** Every write the device owes the server. Authoritative; the screens draw it. */
@Entity(tableName = "outbox")
data class OutboxRow(
    @PrimaryKey val id: String,
    val kind: Kind,
    val payload: String,
    val createdAt: Long,
    val attempts: Int = 0,
    val nextAt: Long,
    val state: State = State.queued,
    val status: Int? = null,
    val answer: String? = null,
    val error: String? = null,
)

@Entity(tableName = "outbox_files", primaryKeys = ["outboxId", "path"])
data class OutboxFile(val outboxId: String, val path: String, val name: String, val mime: String)

/** What a push payload said was due. Read by the notification and Settings; Part E grows this. */
@Entity(tableName = "moments")
data class MomentRow(@PrimaryKey val id: String, val title: String, val at: Long, val fetchedAt: Long)

/**
 * An answer as the server gave it, kept so it can be shown again. `origin` is
 * part of the key: a phone paired elsewhere must not show the last server's
 * notes. `fetchedAt` is when the body was last known to be current — a `304`
 * moves it, because that is what the screen's "fetched" line claims.
 */
@Entity(tableName = "cache", primaryKeys = ["key", "origin"])
data class CacheRow(val key: String, val origin: String, val etag: String?, val body: String, val fetchedAt: Long)

/** A question that was answered, kept whole, so the answer can be read again without the server. */
@Entity(tableName = "asked")
data class AskedRow(@PrimaryKey val question: String, val origin: String, val body: String, val askedAt: Long)

@Dao
interface CacheDao {
    @Query("SELECT * FROM cache WHERE `key` = :key AND origin = :origin") suspend fun get(key: String, origin: String): CacheRow?
    @Upsert suspend fun put(row: CacheRow)
    @Query("UPDATE cache SET fetchedAt = :now WHERE `key` = :key AND origin = :origin") suspend fun touch(key: String, origin: String, now: Long)
    @Query("DELETE FROM cache WHERE `key` = :key AND origin = :origin") suspend fun delete(key: String, origin: String)
    @Query("DELETE FROM cache WHERE fetchedAt < :before") suspend fun deleteBefore(before: Long)
    @Query("DELETE FROM cache") suspend fun clear()
    @Query("SELECT COUNT(*) FROM cache") suspend fun count(): Int
}

@Dao
interface AskedDao {
    @Upsert suspend fun put(row: AskedRow)
    @Query("SELECT * FROM asked WHERE origin = :origin ORDER BY askedAt DESC LIMIT 50") fun recent(origin: String): Flow<List<AskedRow>>
    @Query("SELECT * FROM asked WHERE question = :q AND origin = :origin") suspend fun get(q: String, origin: String): AskedRow?
    @Query("DELETE FROM asked") suspend fun clear()
}

@Dao
interface OutboxDao {
    @Insert suspend fun insert(row: OutboxRow)
    @Insert suspend fun insertFiles(files: List<OutboxFile>)

    /**
     * A file capture arrives whole or not at all. Inserted separately, a
     * drain pass kicked by an earlier capture could read the row in the gap,
     * send it with no files, and park the 4xx in `held` — the share lost
     * although its bytes were already on disk.
     */
    @Transaction
    suspend fun insertWithFiles(row: OutboxRow, files: List<OutboxFile>) {
        insert(row)
        insertFiles(files)
    }
    @Update suspend fun update(row: OutboxRow)
    @Query("SELECT * FROM outbox WHERE id = :id") suspend fun get(id: String): OutboxRow?
    @Query("SELECT * FROM outbox ORDER BY createdAt DESC") fun all(): Flow<List<OutboxRow>>
    @Query("SELECT * FROM outbox WHERE state = 'queued' AND nextAt <= :now ORDER BY createdAt ASC")
    suspend fun dueQueued(now: Long): List<OutboxRow>
    @Query("SELECT * FROM outbox_files WHERE outboxId = :id") suspend fun filesOf(id: String): List<OutboxFile>
    @Query("UPDATE outbox SET state = 'refused' WHERE state = 'queued'") suspend fun refuseAll()
    @Query("UPDATE outbox SET state = 'queued', nextAt = :now WHERE state = 'refused'") suspend fun requeueRefused(now: Long)
    @Query("DELETE FROM outbox WHERE id = :id") suspend fun delete(id: String)
    @Query("DELETE FROM outbox_files WHERE outboxId = :id") suspend fun deleteFiles(id: String)
    @Query("SELECT id FROM outbox WHERE state = 'sent' AND createdAt < :before") suspend fun sentBefore(before: Long): List<String>
}

@Dao
interface MomentsDao {
    @Upsert suspend fun upsert(rows: List<MomentRow>)
    @Query("SELECT * FROM moments ORDER BY at DESC LIMIT 1") fun latest(): Flow<MomentRow?>
    @Query("SELECT * FROM moments ORDER BY at") suspend fun all(): List<MomentRow>
    @Query("SELECT * FROM moments WHERE id = :id") suspend fun get(id: String): MomentRow?
    @Query("DELETE FROM moments WHERE id NOT IN (:ids)") suspend fun deleteExcept(ids: List<String>)
}

class Converters {
    @TypeConverter fun kindTo(k: Kind) = k.name
    @TypeConverter fun kindFrom(s: String) = Kind.valueOf(s)
    @TypeConverter fun stateTo(s: State) = s.name
    @TypeConverter fun stateFrom(s: String) = State.valueOf(s)
}

@Database(
    entities = [OutboxRow::class, OutboxFile::class, MomentRow::class, CacheRow::class, AskedRow::class],
    version = 5,
    exportSchema = true,
)
@TypeConverters(Converters::class)
abstract class Db : RoomDatabase() {
    abstract fun outboxDao(): OutboxDao
    abstract fun momentsDao(): MomentsDao
    abstract fun cacheDao(): CacheDao
    abstract fun askedDao(): AskedDao

    companion object {
        /** [name] is the mode's: each keeps its outbox and its cache in a file of its own. */
        fun open(context: Context, name: String = "engram.db"): Db =
            Room.databaseBuilder(context, Db::class.java, name)
                .setQueryCoroutineContext(Dispatchers.IO)
                .addMigrations(TO_2, TO_3, TO_4, TO_5)
                .build()

        /**
         * Version 2 adds the two tables reading needs and touches nothing else.
         * Written by hand and not left to a destructive fallback: version 1
         * holds the outbox, and the outbox is the one thing on this device that
         * exists nowhere else.
         */
        val TO_2 = object : Migration(1, 2) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL(
                    "CREATE TABLE IF NOT EXISTS `cache` (`key` TEXT NOT NULL, `origin` TEXT NOT NULL, `etag` TEXT, " +
                        "`body` TEXT NOT NULL, `fetchedAt` INTEGER NOT NULL, PRIMARY KEY(`key`, `origin`))",
                )
                db.execSQL(
                    "CREATE TABLE IF NOT EXISTS `asked` (`question` TEXT NOT NULL, `origin` TEXT NOT NULL, " +
                        "`body` TEXT NOT NULL, `askedAt` INTEGER NOT NULL, PRIMARY KEY(`question`))",
                )
            }
        }

        /**
         * Version 3 adds no table and alters no column. The outbox's `kind` is
         * stored as text, so the judging answers are new words in a column
         * that already holds words. The version moves anyway, because the file
         * has to say which vocabulary its rows are written in, and a step that
         * is not written down is a step Room takes destructively.
         */
        val TO_3 = object : Migration(2, 3) {
            override fun migrate(db: SupportSQLiteDatabase) {}
        }

        /** Version 4 is one more word in `kind` — `artifact_delete` — and, as with 3, nothing else. */
        val TO_4 = object : Migration(3, 4) {
            override fun migrate(db: SupportSQLiteDatabase) {}
        }

        /** Version 5 is the last such word — `call` — after which a new route is a new row, not a new version. */
        val TO_5 = object : Migration(4, 5) {
            override fun migrate(db: SupportSQLiteDatabase) {}
        }

        /**
         * Tests: in memory, on the platform's SQLite, with whatever Context the
         * runner lends. Robolectric supplies a real SQLite on the JVM, which is
         * why the bundled driver was not needed after all.
         */
        fun inMemory(context: Context): Db =
            Room.inMemoryDatabaseBuilder(context, Db::class.java)
                .setQueryCoroutineContext(Dispatchers.IO)
                .build()
    }
}
