package io.unom.punktfunk

import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.library.InstallOutcome
import io.unom.punktfunk.kit.library.MgmtReply
import io.unom.punktfunk.kit.say
import org.junit.Assert.assertEquals
import org.junit.Test

/** A mgmt failure reads as the host's own sentence, else its status; never exception text. */
class MgmtReplyTest {
    @Test
    fun the_host_sentence_comes_from_the_api_error_envelope() {
        val refused = MgmtReply.Answer.of(409, """{"error":"Quit Halo first."}""")
        assertEquals(Say.Text("Quit Halo first."), refused.why)
        assertEquals(
            InstallOutcome.Refused("Quit Halo first."),
            InstallOutcome.fromReply(refused.code, refused.apiError),
        )
        assertEquals(say(R.string.error_mgmt_refused, 500), MgmtReply.Answer.of(500, "<html>").why)
        assertEquals(say(R.string.android_mgmt_unreachable), MgmtReply.Unreachable.why)
    }
}
