import { open, fromFile } from '@botejs/core'
import { publish } from './message-bus'

await using cursor = await open(fromFile('./some-large.json'))

for await (const orders of cursor.iter('orders', {
  batch: 10,
  select: { id: 'id', status: 'status', total: ['payment', 'total'] },
  withKey: true,
})) {
  const messages = await Promise.all(
    orders.map(async ([index, order]) => {
      if (order.status !== 'paid') {
        return null
      }

      const items = await cursor.hop('orders', index, 'items')
      const restock = []
      for await (const batch of items.iter({ withKey: true, select: 'sku' })) {
        for (const [sku] of batch) {
          const line = await items.hop(sku)
          if (!(await line.has('backorder'))) {
            continue
          }

          const short = await line.get('backorder', 'shortfall')

          // `availability` is an object of scalar counts, so iter yields the
          // [warehouse, onHand] pairs directly - no per-member cursor needed.
          const sources = []
          for await (const avail of line.iter('availability', { withKey: true })) {
            for (const [warehouse, onHand] of avail) {
              if (onHand > 0) {
                sources.push({ warehouse, onHand })
              }
            }
          }

          restock.push({ sku, short, sources })
        }
      }

      return restock.length ? { id: order.id, total: order.total, restock } : null
    }),
  )

  for (const message of messages) {
    if (message) {
      await publish('orders.fulfil', message)
    }
  }
}
