package io.unom.punktfunk.kit.library

import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.say
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

// Only a game this device launched and still streams is the in-stream End game's; the words
// match the Rust, Swift and web clients.
class RunningGameTest {
    @Test
    fun streamedHereIsThisDevicesLiveLaunch() {
        val games = LibraryClient.parseRunning(
            """{"games":[
              {"app_id":"steam:1","title":"A","state":"running","session_id":3,"endable":true},
              {"app_id":"steam:2","title":"B","state":"running","session_id":4},
              {"app_id":"steam:3","title":"C","state":"detached","endable":true}
            ]}""",
        )
        assertTrue(games[0].streamedHere)
        assertFalse("another device's launch", games[1].streamedHere)
        assertFalse("nobody streams it", games[2].streamedHere)
        assertTrue(games[2].endable && games[2].isUp)
    }

    @Test
    fun aGameEndStatusMapsToWhatThePlayerIsTold() {
        assertEquals(GameEnd.Ended, GameEnd.fromStatus(200))
        assertEquals(GameEnd.NotRunning, GameEnd.fromStatus(409))
        assertEquals(GameEnd.Unsupported, GameEnd.fromStatus(401))
        assertEquals(GameEnd.Unsupported, GameEnd.fromStatus(404))
        assertEquals(GameEnd.Expired, GameEnd.fromStatus(403))
        assertEquals(say(R.string.lib_end_game_not_running, "Hades"), GameEnd.NotRunning.notice("Hades"))
    }
}
