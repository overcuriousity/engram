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
import androidx.room.TypeConverter
import androidx.room.TypeConverters
import androidx.room.Update
import androidx.room.Upsert
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

@Dao
interface OutboxDao {
    @Insert suspend fun insert(row: OutboxRow)
    @Insert suspend fun insertFiles(files: List<OutboxFile>)
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

@Database(entities = [OutboxRow::class, OutboxFile::class, MomentRow::class], version = 1, exportSchema = true)
@TypeConverters(Converters::class)
abstract class Db : RoomDatabase() {
    abstract fun outboxDao(): OutboxDao
    abstract fun momentsDao(): MomentsDao

    companion object {
        fun open(context: Context): Db =
            Room.databaseBuilder(context, Db::class.java, "engram.db")
                .setQueryCoroutineContext(Dispatchers.IO)
                .build()

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
