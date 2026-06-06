package dev.cq27.heldmini

class MainBridge {
  val bridgeName: String = "held-mini"

  fun openDatabase() = Unit

  suspend fun syncOnce() = Unit

  companion object {
    const val DEFAULT_NAME = "held"

    fun create() = MainBridge()
  }
}

object BridgeRegistry {
  val active = true
}
