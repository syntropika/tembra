package com.tembra

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class TembraKernelTest {
    @Test
    fun kernelsKeepIndependentStateAndCloseOnce() {
        TembraKernel().use { first ->
            TembraKernel().use { second ->
                assertEquals("{\"ok\":1}", first.request("{\"op\":\"allocate\"}"))
                assertEquals("{\"ok\":1}", second.request("{\"op\":\"allocate\"}"))
                assertTrue(first.request("invalid").contains("\"error\""))
                first.close()
                first.close()
                assertThrows(IllegalStateException::class.java) { first.request("{}") }
                assertEquals("{\"ok\":2}", second.request("{\"op\":\"allocate\"}"))
            }
        }
    }
}
