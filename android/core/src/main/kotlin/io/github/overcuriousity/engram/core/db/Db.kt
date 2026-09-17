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

enum class Kind { capture_text, capture_files, done, snooze }
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
}

class Converters {
    @TypeConverter fun kindTo(k: Kind) = k.name
    @TypeConverter fun kindFrom(s: String) = Kind.valueOf(s)
    @TypeConverter fun stateTo(s: State) = s.name
    @TypeConverter fun stateFrom(s: String) = State.valueOf(s)
}

@Database(
    entities = [OutboxRow::class, OutboxFile::class, MomentRow::class, CacheRow::class, AskedRow::class],
    version = 2,
    exportSchema = true,
)
@TypeConverters(Converters::class)
abstract class Db : RoomDatabase() {
    abstract fun outboxDao(): OutboxDao
    abstract fun momentsDao(): MomentsDao
    abstract fun cacheDao(): CacheDao
    abstract fun askedDao(): AskedDao

    companion object {
        fun open(context: Context): Db =
            Room.databaseBuilder(context, Db::class.java, "engram.db")
                .setQueryCoroutineContext(Dispatchers.IO)
                .addMigrations(TO_2)
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
