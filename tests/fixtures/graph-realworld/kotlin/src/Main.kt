package dev.cq27.graph

import dev.cq27.external.ExternalFactory

object SingletonRunner {
    fun run() {}
}

class Worker(val name: String) {
    companion object {
        fun create(): Worker = Worker("created")
    }
}

fun String.cleaned(): String = trim()

suspend fun syncOnce() {
    val worker = Worker.create()
    SingletonRunner.run()
    ExternalFactory()
    worker.name.cleaned()
}
